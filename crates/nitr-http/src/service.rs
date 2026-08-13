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
use crate::request::LuaRequest;
use nitr_core::Error;
use nitr_core::Result;
use nitr_core::RuntimePool;

/// Service that handles incoming requests by checking a Lua runtime out of
/// the pool for the duration of each request.
pub struct Svc {
    pool: Arc<RuntimePool>,
    /// Streaming-response slots (`max_streams`); a permit is held for each
    /// live streaming body.
    streams: Arc<Semaphore>,
    peer_addr: SocketAddr,
}

impl Svc {
    pub fn new(pool: Arc<RuntimePool>, streams: Arc<Semaphore>, peer_addr: SocketAddr) -> Self {
        Self {
            pool,
            streams,
            peer_addr,
        }
    }
}

impl Service<Request<Incoming>> for Svc {
    type Response = Response<BoxBody<Bytes, Infallible>>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        let pool = self.pool.clone();
        let streams = self.streams.clone();
        let req = LuaRequest(self.peer_addr, req, Vec::new());

        Box::pin(async move { handler::handle(&pool, req, streams).await })
    }
}
