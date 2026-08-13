use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

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
    peer_addr: SocketAddr,
}

impl Svc {
    pub fn new(pool: Arc<RuntimePool>, peer_addr: SocketAddr) -> Self {
        Self { pool, peer_addr }
    }
}

impl Service<Request<Incoming>> for Svc {
    type Response = Response<BoxBody<Bytes, Infallible>>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        let pool = self.pool.clone();
        let req = LuaRequest(self.peer_addr, req);

        Box::pin(async move { handler::handle(&pool, req).await })
    }
}
