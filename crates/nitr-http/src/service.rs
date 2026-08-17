use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Semaphore;

use http_body_util::combinators::BoxBody;
use hyper::body::{Bytes, Incoming};
use hyper::service::Service;
use hyper::{Request, Response};

use crate::handler;
use crate::protect::Protection;
use crate::request::LuaRequest;
use nitr_core::Error;
use nitr_core::Result;
use nitr_core::RuntimePool;
use tracing::Instrument as _;

/// Service that handles incoming requests by checking a Lua runtime out of
/// the pool for the duration of each request.
pub struct Svc {
    /// The swappable pool handle: read per request so reloads apply to
    /// live keep-alive connections too.
    pool: Arc<std::sync::RwLock<Arc<RuntimePool>>>,
    /// Streaming-response slots (`max_streams`); a permit is held for each
    /// live streaming body.
    streams: Arc<Semaphore>,
    /// Pre-Lua protection (rate limiting, size limits) and request ids.
    protection: Arc<Protection>,
    peer_addr: SocketAddr,
}

impl Svc {
    pub(crate) fn new(
        pool: Arc<std::sync::RwLock<Arc<RuntimePool>>>,
        streams: Arc<Semaphore>,
        protection: Arc<Protection>,
        peer_addr: SocketAddr,
    ) -> Self {
        Self {
            pool,
            streams,
            protection,
            peer_addr,
        }
    }
}

impl Service<Request<Incoming>> for Svc {
    type Response = Response<BoxBody<Bytes, Infallible>>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        let pool = crate::server::current_pool(&self.pool);
        let streams = self.streams.clone();
        let protection = self.protection.clone();
        let id = protection.request_id(&req);
        // The per-request span: every tracing event below it (including
        // Lua `log.*` calls) carries the request id, method, and path.
        let span = tracing::info_span!(
            "request",
            id = %id,
            method = %req.method(),
            path = %req.uri().path(),
        );
        let req = LuaRequest {
            peer_addr: self.peer_addr,
            req: req.map(|body| {
                use http_body_util::BodyExt as _;
                body.map_err(|err| Box::new(err) as _).boxed()
            }),
            params: Vec::new(),
            id,
            // Replaced with the configured bounds by the handler.
            limits: Default::default(),
            cached_form: None,
        };

        Box::pin(
            async move { handler::handle(&pool, req, streams, protection).await }.instrument(span),
        )
    }
}
