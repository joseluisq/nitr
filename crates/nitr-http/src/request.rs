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

/// Whether the client's cached copy is still current.
///
/// `If-None-Match` wins over `If-Modified-Since` when both are present,
/// which is what RFC 9110 requires: an entity tag is an exact identifier
/// and a date is a heuristic.
pub(crate) fn is_fresh(
    headers: &hyper::HeaderMap,
    etag: Option<&str>,
    last_modified: Option<i64>,
) -> bool {
    if let Some(candidates) = headers
        .get(hyper::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        let Some(etag) = etag else {
            return false;
        };
        return candidates.split(',').any(|candidate| {
            let candidate = candidate.trim();
            // `*` matches any existing representation, and the weak/strong
            // prefix is not part of the comparison this header calls for.
            candidate == "*" || strip_weak(candidate) == strip_weak(etag)
        });
    }

    let (Some(since), Some(modified)) = (
        headers
            .get(hyper::header::IF_MODIFIED_SINCE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| httpdate::parse_http_date(v).ok()),
        last_modified,
    ) else {
        return false;
    };
    let since = since
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    // HTTP dates have second precision, so compare truncated.
    modified <= since
}

fn strip_weak(etag: &str) -> &str {
    etag.strip_prefix("W/").unwrap_or(etag)
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
    /// Body-parsing bounds, copied from `[limits]` when the request is
    /// dispatched. Carried on the request because the Lua-facing parsers
    /// need them and Lua must not be able to raise them.
    pub(crate) limits: FormLimits,
}

/// Bounds applied while parsing a request body into Lua values.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FormLimits {
    pub(crate) max_parts: usize,
    pub(crate) max_field_bytes: u64,
    pub(crate) max_file_bytes: u64,
}

impl Default for FormLimits {
    fn default() -> Self {
        let defaults = crate::config::LimitsConfig::default();
        Self {
            max_parts: defaults.max_form_parts,
            max_field_bytes: defaults.max_field_bytes,
            max_file_bytes: defaults.max_file_bytes,
        }
    }
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

        // req:read()  — the next chunk as it arrives off the socket.
        // req:read(n) — at least n bytes, or fewer at the end of the body.
        //
        // The size argument is what lets a handler process an upload larger
        // than its own memory limit: the request-side mirror of a streaming
        // response. `nil` marks the end of the body.
        methods.add_async_method_mut("read", |lua, mut req, n: Option<usize>| async move {
            let reader = req.req.body_mut();
            let Some(want) = n else {
                while let Some(frame) = reader.frame().await {
                    // Trailer frames carry no data; keep reading.
                    if let Some(bytes) = frame.into_lua_err()?.data_ref() {
                        return Some(lua.create_string(bytes)).transpose();
                    }
                }
                return Ok(None);
            };

            let mut buf = Vec::new();
            while buf.len() < want {
                let Some(frame) = reader.frame().await else {
                    break;
                };
                if let Some(bytes) = frame.into_lua_err()?.data_ref() {
                    buf.extend_from_slice(bytes);
                }
            }
            if buf.is_empty() {
                return Ok(None);
            }
            Some(lua.create_string(buf)).transpose()
        });

        // req:form() — an `application/x-www-form-urlencoded` body as a
        // table. Percent-decoding and `+`-as-space are HTTP details worth
        // exactly one careful implementation, not one per application.
        // Repeated keys keep the last value, matching `req.query`.
        methods.add_async_method_mut("form", |lua, mut req, ()| async move {
            let body = req
                .req
                .body_mut()
                .collect()
                .await
                .into_lua_err()?
                .to_bytes();
            let table = lua.create_table()?;
            for (k, v) in url::form_urlencoded::parse(&body) {
                table.set(k.as_ref(), v.as_ref())?;
            }
            Ok(table)
        });

        // req:multipart(fn) — invokes `fn` once per part, in arrival order.
        // See `crate::multipart` for why parts stream instead of being
        // collected. Returns the number of parts seen.
        methods.add_async_method_mut("multipart", |lua, mut req, cb: mlua::Function| async move {
            let content_type = req
                .req
                .headers()
                .get(hyper::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let boundary = crate::multipart::boundary(content_type.as_deref())?;
            let limits = req.limits;

            let body = std::mem::take(req.req.body_mut());
            let mut parser = multer::Multipart::new(body.into_data_stream(), boundary);

            let mut count = 0usize;
            loop {
                let Some(field) = parser.next_field().await.into_lua_err()? else {
                    break;
                };
                count += 1;
                if count > limits.max_parts {
                    return Err(mlua::Error::RuntimeError(format!(
                        "multipart body has more than {} parts",
                        limits.max_parts
                    )));
                }
                let part = lua.create_userdata(crate::multipart::LuaPart::new(
                    field,
                    limits.max_field_bytes,
                    limits.max_file_bytes,
                ))?;
                let outcome = cb.call_async::<()>(&part).await;

                // The parser cannot advance while a field is alive, so the
                // part is reclaimed and drained whatever the callback did
                // with it — including nothing, and including failing.
                if let Ok(part) = part.borrow::<crate::multipart::LuaPart>() {
                    if let Some(mut field) = part.reclaim() {
                        while field.chunk().await.into_lua_err()?.is_some() {}
                    }
                }
                outcome?;
            }
            Ok(count)
        });

        // req:fresh(etag, last_modified?) — whether the client's cached
        // copy is still current. Rust compares the validators; Lua decides
        // what identifies the resource, which is application knowledge.
        methods.add_method(
            "fresh",
            |_, req, (etag, last_modified): (Option<String>, Option<i64>)| {
                Ok(is_fresh(req.req.headers(), etag.as_deref(), last_modified))
            },
        );

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

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> hyper::HeaderMap {
        let mut map = hyper::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().expect("header value"));
        }
        map
    }

    #[test]
    fn if_none_match_compares_ignoring_weakness() {
        let h = headers(&[("if-none-match", "\"abc\"")]);
        assert!(is_fresh(&h, Some("\"abc\""), None));
        assert!(is_fresh(&h, Some("W/\"abc\""), None));
        assert!(!is_fresh(&h, Some("\"other\""), None));
        // No validator to compare against is not a match.
        assert!(!is_fresh(&h, None, None));
    }

    #[test]
    fn if_none_match_handles_lists_and_the_wildcard() {
        let list = headers(&[("if-none-match", "\"a\", \"b\" , \"c\"")]);
        assert!(is_fresh(&list, Some("\"b\""), None));
        assert!(!is_fresh(&list, Some("\"d\""), None));

        let any = headers(&[("if-none-match", "*")]);
        assert!(is_fresh(&any, Some("\"anything\""), None));
    }

    #[test]
    fn if_modified_since_applies_only_without_an_entity_tag() {
        let stamp = 1_700_000_000i64;
        let date = httpdate::fmt_http_date(
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(stamp as u64),
        );

        let only_date = headers(&[("if-modified-since", &date)]);
        assert!(is_fresh(&only_date, None, Some(stamp)));
        assert!(is_fresh(&only_date, None, Some(stamp - 60)));
        assert!(!is_fresh(&only_date, None, Some(stamp + 60)));

        // With both present the entity tag decides, even when the date
        // would have said "fresh".
        let both = headers(&[("if-none-match", "\"x\""), ("if-modified-since", &date)]);
        assert!(!is_fresh(&both, Some("\"y\""), Some(stamp)));
    }

    #[test]
    fn a_request_without_validators_is_never_fresh() {
        assert!(!is_fresh(
            &hyper::HeaderMap::new(),
            Some("\"abc\""),
            Some(1)
        ));
    }
}
