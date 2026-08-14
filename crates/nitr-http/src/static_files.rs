//! Static file serving, entirely in Rust: requests that match a static
//! mount never touch a Lua state. Supports conditional requests
//! (`ETag` / `Last-Modified` → 304), content-type detection, directory
//! `index.html`, an SPA fallback, and streamed file bodies.
//!
//! Path safety: the URL path is percent-decoded, split into components
//! (rejecting `..`, absolute and empty segments), joined under the mount
//! directory, and the final canonicalized path must stay inside the
//! canonicalized root — so symlinks cannot escape the mount either.

use std::convert::Infallible;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use http_body_util::{BodyExt as _, Empty, Full, StreamBody};
use hyper::body::{Bytes, Frame};
use hyper::header::{self, HeaderValue};
use hyper::{Method, Response, StatusCode};

use crate::handler::HttpResponse;
use crate::request::LuaRequest;
use nitr_core::Result;

/// Chunk size for streamed file bodies.
const FILE_CHUNK: usize = 64 * 1024;

/// Files up to this size are served in one piece instead of streamed.
const INLINE_LIMIT: u64 = 256 * 1024;

/// One static mount: requests under `mount` are served from `dir`.
#[derive(Debug, Clone)]
pub(crate) struct StaticMount {
    /// URL prefix, normalized to start with `/` and not end with one
    /// (except the root mount `/`).
    pub(crate) mount: String,
    pub(crate) dir: PathBuf,
    /// Serve `index.html` for unknown paths (single-page applications).
    pub(crate) spa: bool,
    /// Explicit `Cache-Control` header value for served files.
    pub(crate) cache_control: Option<String>,
}

impl StaticMount {
    pub(crate) fn new(
        mount: impl Into<String>,
        dir: impl Into<PathBuf>,
        spa: bool,
        cache_control: Option<String>,
    ) -> Self {
        let mut mount = mount.into();
        if !mount.starts_with('/') {
            mount.insert(0, '/');
        }
        while mount.len() > 1 && mount.ends_with('/') {
            mount.pop();
        }
        Self {
            mount,
            dir: dir.into(),
            spa,
            cache_control,
        }
    }

    /// The request path relative to this mount, when it applies.
    fn relative<'p>(&self, path: &'p str) -> Option<&'p str> {
        if self.mount == "/" {
            return Some(path.trim_start_matches('/'));
        }
        let rest = path.strip_prefix(&self.mount)?;
        match rest.as_bytes().first() {
            None => Some(""),
            Some(b'/') => Some(&rest[1..]),
            _ => None, // /assetsfoo must not match mount /assets
        }
    }
}

/// The `[static]` configuration expressed as mounts (empty when no `dir`
/// is configured).
pub(crate) fn base_mounts(cfg: &crate::config::Config) -> Vec<StaticMount> {
    cfg.static_files
        .dir
        .as_ref()
        .map(|dir| {
            vec![StaticMount::new(
                cfg.static_files.mount.clone().unwrap_or_else(|| "/".into()),
                dir.clone(),
                cfg.static_files.spa,
                cfg.static_files.cache_control.clone(),
            )]
        })
        .unwrap_or_default()
}

/// Tries to serve the request from the given mounts (first match on the
/// longest mount prefix wins). `None` means "not a static asset" and the
/// caller continues its normal dispatch.
pub(crate) async fn try_serve(
    mounts: &[StaticMount],
    req: &LuaRequest,
) -> Option<Result<HttpResponse>> {
    if mounts.is_empty() || !matches!(*req.req.method(), Method::GET | Method::HEAD) {
        return None;
    }
    let path = req.req.uri().path();
    let decoded = percent_encoding::percent_decode_str(path)
        .decode_utf8()
        .ok()?;

    let mut candidates: Vec<&StaticMount> = mounts
        .iter()
        .filter(|m| m.relative(&decoded).is_some())
        .collect();
    candidates.sort_by_key(|m| std::cmp::Reverse(m.mount.len()));

    for mount in candidates {
        let rel = mount.relative(&decoded)?;
        let Some(file) = resolve(&mount.dir, rel).await else {
            // Unknown path inside an SPA mount falls back to its index.
            if mount.spa {
                if let Some(index) = resolve(&mount.dir, "index.html").await {
                    return Some(serve_file(req, mount, &index).await);
                }
            }
            continue;
        };
        return Some(serve_file(req, mount, &file).await);
    }
    None
}

/// Resolves a relative URL path to a regular file inside `dir`, or `None`
/// (unsafe path, missing file, unreadable metadata). Directories resolve
/// to their `index.html`.
async fn resolve(dir: &Path, rel: &str) -> Option<PathBuf> {
    let mut path = dir.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        // Reject traversal and any non-normal component (`..`, drive
        // prefixes, absolute segments).
        match Path::new(part).components().next() {
            Some(Component::Normal(component)) if component == part => path.push(part),
            None => continue,
            _ => return None,
        }
    }

    let meta = tokio::fs::metadata(&path).await.ok()?;
    if meta.is_dir() {
        path.push("index.html");
        tokio::fs::metadata(&path)
            .await
            .ok()?
            .is_file()
            .then_some(())?;
    } else if !meta.is_file() {
        return None;
    }

    // Symlink policy: the canonical target must stay inside the canonical
    // root, so links cannot escape the mount.
    let canonical = tokio::fs::canonicalize(&path).await.ok()?;
    let root = tokio::fs::canonicalize(dir).await.ok()?;
    canonical.starts_with(&root).then_some(canonical)
}

/// Serves one resolved file with conditional-request support.
async fn serve_file(req: &LuaRequest, mount: &StaticMount, path: &Path) -> Result<HttpResponse> {
    let meta = match tokio::fs::metadata(path).await {
        Ok(meta) => meta,
        Err(err) => {
            tracing::error!("failed to stat static file {}: {err}", path.display());
            return not_found();
        }
    };
    let len = meta.len();
    let modified = meta.modified().ok();
    let etag = etag_for(len, modified);

    // Conditional requests: If-None-Match wins over If-Modified-Since.
    let headers = req.req.headers();
    let not_modified = match headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        Some(candidates) => candidates
            .split(',')
            .any(|candidate| candidate.trim() == etag),
        None => match (
            headers
                .get(header::IF_MODIFIED_SINCE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| httpdate::parse_http_date(v).ok()),
            modified,
        ) {
            // HTTP dates have second precision; compare truncated.
            (Some(since), Some(modified)) => secs_since_epoch(modified) <= secs_since_epoch(since),
            _ => false,
        },
    };

    let mut builder = Response::builder().status(if not_modified {
        StatusCode::NOT_MODIFIED
    } else {
        StatusCode::OK
    });
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    builder = builder
        .header(header::CONTENT_TYPE, mime.as_ref())
        .header(header::ETAG, &etag);
    if let Some(modified) = modified {
        builder = builder.header(header::LAST_MODIFIED, httpdate::fmt_http_date(modified));
    }
    if let Some(cache_control) = &mount.cache_control {
        if let Ok(value) = HeaderValue::from_str(cache_control) {
            builder = builder.header(header::CACHE_CONTROL, value);
        }
    }

    if not_modified {
        return Ok(builder.body(Empty::<Bytes>::new().boxed())?);
    }
    builder = builder.header(header::CONTENT_LENGTH, len);
    if *req.req.method() == Method::HEAD {
        return Ok(builder.body(Empty::<Bytes>::new().boxed())?);
    }

    if len <= INLINE_LIMIT {
        match tokio::fs::read(path).await {
            Ok(data) => Ok(builder.body(Full::new(Bytes::from(data)).boxed())?),
            Err(err) => {
                tracing::error!("failed to read static file {}: {err}", path.display());
                not_found()
            }
        }
    } else {
        Ok(builder.body(stream_file(path.to_path_buf()).await?)?)
    }
}

/// Streams a large file through a small bounded channel (same shape as
/// streaming Lua bodies); a read error mid-stream closes the body.
async fn stream_file(
    path: PathBuf,
) -> Result<http_body_util::combinators::BoxBody<Bytes, Infallible>> {
    use tokio::io::AsyncReadExt as _;

    let mut file = tokio::fs::File::open(&path).await.map_err(|err| {
        nitr_core::Error::Io(std::io::Error::new(
            err.kind(),
            format!("failed to open static file {}: {err}", path.display()),
        ))
    })?;
    let (tx, rx) = async_channel::bounded::<std::result::Result<Frame<Bytes>, Infallible>>(2);
    tokio::spawn(async move {
        let mut buf = vec![0u8; FILE_CHUNK];
        loop {
            match file.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = Bytes::copy_from_slice(&buf[..n]);
                    if tx.send(Ok(Frame::data(chunk))).await.is_err() {
                        break; // client disconnected
                    }
                }
                Err(err) => {
                    tracing::error!("static file read failed mid-stream: {err}");
                    break;
                }
            }
        }
        tx.close();
    });
    Ok(StreamBody::new(rx).boxed())
}

fn etag_for(len: u64, modified: Option<SystemTime>) -> String {
    format!("\"{len:x}-{:x}\"", modified.map_or(0, secs_since_epoch))
}

fn secs_since_epoch(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn not_found() -> Result<HttpResponse> {
    crate::handler::plain_response(StatusCode::NOT_FOUND, "Not Found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mounts_normalize_and_match() {
        let m = StaticMount::new("assets/", "public", false, None);
        assert_eq!(m.mount, "/assets");
        assert_eq!(m.relative("/assets"), Some(""));
        assert_eq!(m.relative("/assets/app.js"), Some("app.js"));
        assert_eq!(m.relative("/assetsfoo"), None);
        assert_eq!(m.relative("/other"), None);

        let root = StaticMount::new("/", "public", false, None);
        assert_eq!(root.relative("/x/y.css"), Some("x/y.css"));
    }

    #[tokio::test]
    async fn traversal_and_escapes_are_rejected() {
        let dir = std::env::temp_dir().join(format!("nitr-static-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).expect("mkdir");
        std::fs::write(dir.join("ok.txt"), b"ok").expect("write");
        std::fs::write(dir.join("sub/inner.txt"), b"inner").expect("write");

        assert!(resolve(&dir, "ok.txt").await.is_some());
        assert!(resolve(&dir, "sub/inner.txt").await.is_some());
        assert!(resolve(&dir, "../etc/passwd").await.is_none());
        assert!(resolve(&dir, "sub/../../etc/passwd").await.is_none());
        assert!(resolve(&dir, "/etc/passwd").await.is_none());
        assert!(resolve(&dir, "missing.txt").await.is_none());

        std::fs::remove_dir_all(&dir).ok();
    }
}
