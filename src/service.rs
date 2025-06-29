use http_body_util::combinators::BoxBody;
use hyper::body::{Bytes, Incoming};
use hyper::service::Service;
use hyper::{Request, Response};
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use crate::error::Result;
use crate::handler;
use crate::runtime::Runtime;
use crate::userdata::request::LuaRequest;
use crate::Error;

/// Service that handles incoming requests
pub struct Svc {
    rt: Arc<Runtime>,
    peer_addr: SocketAddr,
}

impl Svc {
    pub fn new(rt: Arc<Runtime>, peer_addr: SocketAddr) -> Self {
        Self { rt, peer_addr }
    }
}

impl Service<Request<Incoming>> for Svc {
    type Response = Response<BoxBody<Bytes, Infallible>>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        let rt = self.rt.clone();
        let req = LuaRequest(self.peer_addr, req);

        Box::pin(async move { handler::handle(&rt, req).await })
    }
}
