use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use http_body_util::combinators::BoxBody;
use http_body_util::BodyExt as _;
use hyper::body::{Body, Bytes, Frame};
use hyper::Request;

/// The request body type: boxed so both real (`hyper::body::Incoming`) and
/// synthetic (test client) bodies flow through the same dispatch.
pub(crate) type IncomingBody = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;
use mlua::{ExternalResult, LuaSerdeExt, UserData, UserDataFields, UserDataMethods};
use serde_json::Value as SerdeValue;

struct LimitedBody {
    inner: IncomingBody,
    limit: u64,
    read: u64,
    exceeded: Arc<AtomicBool>,
}

impl Body for LimitedBody {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Self::Data>, Self::Error>>> {
        // `BoxBody` holds a pinned box, so the wrapper is `Unpin` and the
        // projection is a plain borrow.
        let this = self.get_mut();
        let frame = match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => frame,
            other => return other,
        };
        if let Some(data) = frame.data_ref() {
            this.read += data.len() as u64;
            if this.read > this.limit {
                this.exceeded.store(true, Ordering::Relaxed);
                return Poll::Ready(Some(Err(Box::new(BodyTooLarge(this.limit)))));
            }
        }
        Poll::Ready(Some(Ok(frame)))
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

/// The error a body read fails with once the ceiling is passed.
#[derive(Debug)]
struct BodyTooLarge(u64);

impl std::fmt::Display for BodyTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "request body exceeded the {} byte limit", self.0)
    }
}

impl std::error::Error for BodyTooLarge {}

/// Wrapper around the incoming request that implements UserData.
pub(crate) struct LuaRequest {
    pub(crate) peer_addr: SocketAddr,
    pub(crate) req: Request<IncomingBody>,
    /// Path parameters captured by the router (empty for the catch-all).
    pub(crate) params: Vec<(String, String)>,
    /// The request id: generated per request (UUIDv7), or taken from a
    /// trusted inbound `X-Request-ID` header.
    pub(crate) id: String,
}

impl LuaRequest {
    /// Caps this request's body at `limit` bytes *as it arrives*.
    ///
    /// The `Content-Length` check in
    /// [`Protection`](crate::protect::Protection) only sees what the client
    /// declared; a chunked body declares nothing and a dishonest one declares
    /// the wrong thing. This counts what actually shows up and fails the
    /// stream the moment it passes the ceiling, so an oversized upload is cut
    /// mid-flight instead of being buffered in full.
    ///
    /// The overflow is also recorded in the returned flag, because by the
    /// time the failure surfaces it has crossed into Lua and become an opaque
    /// error value. The flag lets the handler answer 413 instead of a
    /// generic 500.
    pub(crate) fn limit_body(&mut self, limit: u64) -> Arc<AtomicBool> {
        let exceeded = Arc::new(AtomicBool::new(false));
        let inner = std::mem::take(self.req.body_mut());
        *self.req.body_mut() = LimitedBody {
            inner,
            limit,
            read: 0,
            exceeded: exceeded.clone(),
        }
        .boxed();
        exceeded
    }

    /// Releases the unread remainder of the body.
    ///
    /// The request outlives the response as a Lua userdata — unreachable,
    /// but not collected until the state's next GC — and with it hyper's
    /// `Incoming`, which keeps the exchange open on the connection. A
    /// handler that never read the body, or stopped half-way (an oversized
    /// upload), would otherwise stall that connection until an unrelated
    /// collection happened to run.
    pub(crate) fn discard_body(&mut self) {
        *self.req.body_mut() = BoxBody::default();
    }
}

impl UserData for LuaRequest {
    fn add_fields<'lua, F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("remote_addr", |_, req| Ok(req.peer_addr.to_string()));
        fields.add_field_method_get("method", |_, req| Ok(req.req.method().to_string()));
        fields.add_field_method_get("path", |_, req| Ok(req.req.uri().path().to_string()));
        fields.add_field_method_get("query", |lua, req| {
            // Query string parsed (and percent-decoded) into a table; for
            // repeated keys the last value wins.
            let table = lua.create_table()?;
            if let Some(query) = req.req.uri().query() {
                for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
                    table.set(k.as_ref(), v.as_ref())?;
                }
            }
            Ok(table)
        });
        fields.add_field_method_get("id", |_, req| Ok(req.id.clone()));
        fields.add_field_method_get("params", |lua, req| {
            // Path parameters captured by the router, e.g. `id` for a route
            // registered as `/users/:id`.
            let table = lua.create_table()?;
            for (k, v) in &req.params {
                table.set(k.as_str(), v.as_str())?;
            }
            Ok(table)
        });
        fields.add_field_method_get("uri", |lua, req| {
            let table = lua.create_table()?;
            let uri = req.req.uri();
            table.set("scheme", uri.scheme_str().unwrap_or_default())?;
            table.set("host", uri.host().unwrap_or_default())?;
            table.set("port", uri.port().map_or(0, |v| v.as_u16()))?;
            table.set("path", uri.path())?;
            table.set("authority", uri.authority().map_or("", |a| a.as_str()))?;
            table.set("query", uri.query().unwrap_or_default())?;
            Ok(table)
        });
        fields.add_field_method_get("headers", |lua, req| {
            let headers = req.req.headers();
            let table = lua.create_table()?;
            for (k, v) in headers.iter() {
                table.set(k.as_str(), v.to_str().unwrap_or_default())?;
            }
            Ok(table)
        });
        fields.add_field_method_get("cookies", |_, req| {
            // All `Cookie` headers, joined so multi-header clients work.
            let header = req
                .req
                .headers()
                .get_all(hyper::header::COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .collect::<Vec<_>>()
                .join("; ");
            Ok(nitr_std::RequestCookies::parse(&header))
        });
    }

    fn add_methods<'lua, M: UserDataMethods<Self>>(methods: &mut M) {
        // Returns the best match among the given media types for the
        // request's `Accept` header, or nil when none is acceptable.
        methods.add_method("accepts", |_, req, offers: mlua::Variadic<String>| {
            let accept = req
                .req
                .headers()
                .get(hyper::header::ACCEPT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("*/*");
            let refs: Vec<&str> = offers.iter().map(String::as_str).collect();
            Ok(nitr_std::best_match(accept, &refs).map(|i| offers[i].clone()))
        });

        methods.add_async_method_mut("read", |lua, mut req, ()| async move {
            let reader = req.req.body_mut();
            if let Some(frame) = reader.frame().await {
                if let Some(bytes) = frame.into_lua_err()?.data_ref() {
                    return Some(lua.create_string(bytes)).transpose();
                }
            }
            Ok(None)
        });

        methods.add_async_method_mut("text", |lua, mut req, ()| async move {
            let reader = req.req.body_mut();
            let body = reader.collect().await.into_lua_err()?;
            lua.create_string(body.to_bytes())
        });

        methods.add_async_method_mut("json", |lua, mut req, ()| async move {
            let reader = req.req.body_mut();
            let collected = reader.collect().await.into_lua_err()?;
            let buf = collected.to_bytes();
            if buf.is_empty() {
                return Err(mlua::Error::external(
                    "Unexpected end of JSON input, probably request body is empty or already consumed",
                ));
            }
            let json = serde_json::from_slice::<SerdeValue>(&buf).into_lua_err()?;
            lua.to_value(&json)
        });
    }
}
