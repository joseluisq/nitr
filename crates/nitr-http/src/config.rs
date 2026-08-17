//! Server configuration (`nitr.toml`), defaults, and environment overrides.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use nitr_core::RuntimeOpts;
use nitr_core::{Error, Result};
use nitr_std::Builtins;

/// Default per-state Lua memory limit in bytes.
const DEFAULT_MEMORY_LIMIT: usize = 8 * 1024 * 1024; // 8 MiB

/// Default wall-clock budget per handler invocation, in milliseconds.
const DEFAULT_EXEC_TIMEOUT_MS: u64 = 30_000;

/// Server configuration, typically loaded from a `nitr.toml` file.
///
/// Precedence (strongest first): CLI flags / builder setters, `NITR_*`
/// environment variables (see [`apply_env()`](Self::apply_env)), the TOML
/// file, and finally the built-in defaults.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Address the server binds to.
    pub listen: SocketAddr,
    /// Lua script executed once per request.
    pub handler_script: PathBuf,
    /// Lua script executed once at startup; its returned table is passed to
    /// the handler on every request.
    pub config_script: Option<PathBuf>,
    /// Directory for the `template` builtin.
    pub templates_dir: Option<PathBuf>,
    /// SQLite database for the `db` builtin (`[database]` section). Accepts
    /// either a bare path (`database = "app.db"`) or a table with the
    /// connection pragmas.
    pub database: Option<DatabaseConfig>,
    /// Number of pooled Lua states. Reserved: takes effect with the runtime
    /// pool (roadmap phase 3).
    pub workers: usize,
    /// Maximum concurrent streaming responses (each holds a pooled state
    /// for its whole lifetime). Defaults to `workers - 1` (at least 1) so
    /// idle streams cannot pin the entire pool.
    pub max_streams: Option<usize>,
    /// Development mode: hot-reload the handler script on change.
    pub dev_mode: bool,
    /// Standard library (`nitr.*`) selection (`[std]` section). When the
    /// feature list is omitted, only the minimal set is enabled; an
    /// explicit list is strict and fails at startup when a listed feature
    /// is missing its configuration (e.g. `template` without
    /// `templates_dir`).
    pub std: StdConfig,
    /// Trust an inbound `X-Request-ID` header (well-formed, <= 64 ASCII
    /// chars) instead of generating a fresh id. Enable only behind a proxy
    /// that sets or sanitizes the header.
    pub trust_request_id: bool,
    /// Request-size and connection limits (`[limits]` section).
    pub limits: LimitsConfig,
    /// Per-client rate limiting (`[rate_limit]` section).
    pub rate_limit: RateLimitConfig,
    /// Outbound-request policy for the `fetch` builtin (`[fetch]` section).
    pub fetch: FetchConfig,
    /// Graceful-shutdown timing (`[shutdown]` section).
    pub shutdown: ShutdownConfig,
    /// Response compression (`[compression]` section).
    pub compression: CompressionConfig,
    /// Cross-origin resource sharing (`[cors]` section).
    pub cors: CorsConfig,
    /// The shared `nitr.cache` (`[cache]` section).
    pub cache: CacheConfig,
    /// Static file serving (`[static]` section).
    #[serde(rename = "static")]
    pub static_files: StaticConfig,
    /// Directory `nitr test` discovers `*.lua` test files in.
    pub tests_dir: Option<PathBuf>,
    /// Lua runtime settings.
    pub lua: LuaConfig,
    /// Health and readiness endpoints (`[health]` section).
    pub health: HealthConfig,
    /// Log output (`[log]` section).
    pub log: LogConfig,
    /// File the server writes its process id to at startup (and removes at
    /// exit), so `nitr reload` and scripts can find the process without
    /// grepping the process table.
    pub pidfile: Option<PathBuf>,
}

/// Health and readiness endpoints (`[health]` section), answered entirely
/// in Rust.
///
/// Two deliberately separate questions: liveness ("is the process alive?")
/// never touches a Lua state — a probe that queued behind a saturated pool
/// would cause the restart it exists to prevent — and readiness ("should
/// it receive traffic?") flips to `503` the moment a graceful drain
/// starts, so a rolling deploy shifts traffic before requests can fail.
/// An application cannot influence either answer.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HealthConfig {
    /// Whether the endpoints are served at all.
    pub enabled: bool,
    /// Liveness path: `200 ok` while the process runs.
    pub liveness: String,
    /// Readiness path: `200 ok` while accepting traffic, `503 draining`
    /// once a graceful shutdown begins.
    pub readiness: String,
    /// A separate address to serve the endpoints on, keeping them off the
    /// public port. When unset they answer on the main listener.
    pub bind: Option<SocketAddr>,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            liveness: "/healthz".into(),
            readiness: "/readyz".into(),
            bind: None,
        }
    }
}

/// Log output (`[log]` section).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    /// Output format.
    pub format: LogFormat,
    /// Minimum level (`trace`/`debug`/`info`/`warn`/`error`), or any
    /// `tracing` filter directive. The `RUST_LOG` environment variable
    /// wins over this; without either, `info` (`debug` in dev mode).
    pub level: Option<String>,
}

/// How log lines are rendered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable single-line text (the default).
    #[default]
    Text,
    /// One JSON object per line, with the request/error fields as real
    /// keys — what makes the structured fields usable by a log shipper.
    Json,
}

/// Standard library selection (`[std]` section): which built-in `nitr.*`
/// modules are exposed to scripts.
///
/// The standard library provides building blocks — scripts opt into the
/// features they need (or replace them with their own modules). Without an
/// explicit list only the minimal set is enabled to keep the footprint
/// small.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StdConfig {
    /// Enabled standard library features. Valid names: `"dbg"`, `"fetch"`,
    /// `"template"`, `"json"`, `"db"`, `"http"`, `"log"`, `"crypto"`,
    /// `"cache"`, `"time"`, `"validate"`, `"base64"`, `"path"`, `"url"`.
    /// `None` enables the minimal default set (`json`, `http`, `log`,
    /// `time`, `validate`, `base64`, `path`, `url`); an explicit list is
    /// strict —
    /// unknown names or a listed feature missing its required setting
    /// (e.g. `db` without `database`) fail at startup.
    pub features: Option<Vec<String>>,
}

/// Request-size and connection limits (`[limits]` section), enforced in
/// Rust before a request reaches Lua.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    /// Maximum declared request body size in bytes (413 beyond it).
    pub max_body_bytes: u64,
    /// Maximum request header buffer in bytes (hyper enforces a floor of
    /// 8 KiB).
    pub max_header_bytes: usize,
    /// Maximum request URI length in bytes (414 beyond it).
    pub max_uri_bytes: usize,
    /// Maximum concurrent TCP connections; the listener stops accepting
    /// while at the cap.
    pub max_connections: usize,
    /// How long a request may wait for a free Lua state before it is shed
    /// with `503` and a `Retry-After`, in milliseconds. `0` waits forever
    /// (the pre-phase-10 behavior). Shedding happens before any Lua runs,
    /// so an overloaded server answers quickly instead of queueing work
    /// nobody is still waiting for.
    pub pool_wait_ms: u64,
    /// Maximum number of parts in a `multipart/form-data` body.
    pub max_form_parts: usize,
    /// Maximum size of a single non-file form field, in bytes. Fields
    /// become Lua strings, so this bounds what a request can push into a
    /// state's heap.
    pub max_field_bytes: u64,
    /// Maximum size of a single uploaded file, in bytes. Files stream to
    /// disk in Rust and never enter the Lua heap, so this is far larger
    /// than [`max_field_bytes`](Self::max_field_bytes).
    pub max_file_bytes: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: 1024 * 1024, // 1 MiB
            max_header_bytes: 16 * 1024, // 16 KiB
            max_uri_bytes: 8 * 1024,     // 8 KiB
            max_connections: 1024,
            // Generous by default: long enough that a brief burst queues
            // rather than sheds, short enough that a saturated server fails
            // fast instead of accumulating doomed requests.
            pool_wait_ms: 5_000,
            max_form_parts: 64,
            max_field_bytes: 64 * 1024,       // 64 KiB
            max_file_bytes: 10 * 1024 * 1024, // 10 MiB
        }
    }
}

/// Graceful-shutdown timing (`[shutdown]` section).
///
/// On `SIGTERM`/`SIGINT` the server stops accepting, lets in-flight
/// requests finish, and only then exits. These bound how long it waits.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShutdownConfig {
    /// Seconds to let ordinary in-flight requests finish.
    pub grace: u64,
    /// Extra seconds granted to streaming and SSE bodies, which can
    /// legitimately outlive a normal request. They are cut at
    /// `grace + stream_grace`.
    pub stream_grace: u64,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            grace: 30,
            stream_grace: 5,
        }
    }
}

impl ShutdownConfig {
    /// Deadline for ordinary in-flight requests.
    pub fn grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.grace)
    }

    /// Total deadline including the extra budget for long-lived bodies.
    pub fn total_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.grace + self.stream_grace)
    }
}

/// Outbound-request policy for the `fetch` builtin (`[fetch]` section).
/// By default, requests to loopback/private/link-local addresses are
/// refused (SSRF protection) and every redirect hop is re-checked.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FetchConfig {
    /// When set, only these exact host names may be fetched.
    pub allowed_hosts: Option<Vec<String>>,
    /// Allow requests to private/loopback/link-local addresses.
    pub allow_private_networks: bool,
    /// Maximum response body accumulated by `resp:text()`/`resp:json()`.
    pub max_response_bytes: u64,
    /// Maximum concurrent requests per `await_all(...)` call.
    pub max_concurrent: usize,
    /// Maximum outbound requests one inbound request may make in total.
    /// `max_concurrent` bounds a single `await_all`; this bounds the whole
    /// handler, including a loop issuing calls one after another. `0`
    /// removes the cap.
    pub max_per_request: u32,
    /// Seconds to wait for a TCP/TLS connection to an upstream.
    pub connect_timeout: f64,
    /// Default total budget per outbound request, in seconds. A per-call
    /// `timeout` option overrides it.
    pub timeout: f64,
    /// Idle connections kept per upstream host.
    pub pool_max_idle_per_host: usize,
    /// Maximum retry attempts a call may ask for. Retries are opt-in per
    /// call and only ever applied to idempotent methods.
    pub max_retries: u32,
    /// Proxy URL for outbound requests. Unset reads the conventional
    /// `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY` environment variables.
    pub proxy: Option<String>,
    /// Ignore the proxy environment variables entirely.
    pub no_proxy: bool,
    /// Forward a W3C `traceparent` header on outbound calls, derived from
    /// the inbound request id, so a request crossing services can be
    /// correlated. Pass-through only: this is not a tracing SDK.
    pub propagate_trace_context: bool,
}

impl Default for FetchConfig {
    fn default() -> Self {
        let defaults = nitr_std::FetchOptions::default();
        Self {
            allowed_hosts: defaults.allowed_hosts,
            allow_private_networks: defaults.allow_private_networks,
            max_response_bytes: defaults.max_response_bytes,
            max_concurrent: defaults.max_concurrent,
            max_per_request: defaults.max_per_request,
            connect_timeout: defaults.connect_timeout.as_secs_f64(),
            timeout: defaults.timeout.as_secs_f64(),
            pool_max_idle_per_host: defaults.pool_max_idle_per_host,
            max_retries: defaults.max_retries,
            proxy: None,
            no_proxy: false,
            propagate_trace_context: false,
        }
    }
}

impl FetchConfig {
    /// The runtime policy handed to the `fetch` builtin.
    pub fn options(&self) -> nitr_std::FetchOptions {
        nitr_std::FetchOptions {
            allowed_hosts: self.allowed_hosts.clone(),
            allow_private_networks: self.allow_private_networks,
            max_response_bytes: self.max_response_bytes,
            max_concurrent: self.max_concurrent.max(1),
            max_per_request: self.max_per_request,
            connect_timeout: std::time::Duration::from_secs_f64(self.connect_timeout.max(0.1)),
            timeout: std::time::Duration::from_secs_f64(self.timeout.max(0.1)),
            pool_max_idle_per_host: self.pool_max_idle_per_host,
            max_retries: self.max_retries,
            proxy: self.proxy.clone(),
            no_proxy: self.no_proxy,
            propagate_trace_context: self.propagate_trace_context,
        }
    }
}

/// Static file serving (`[static]` section): requests under `mount` are
/// served from `dir` entirely in Rust, before any Lua dispatch. Scripts
/// can add further mounts with `app:static(mount, dir, opts?)`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticConfig {
    /// Directory served as static files; unset disables the section.
    pub dir: Option<PathBuf>,
    /// URL prefix the directory is mounted at (default `/`).
    pub mount: Option<String>,
    /// Serve `index.html` for unknown paths (single-page applications).
    pub spa: bool,
    /// `Cache-Control` header value for served files.
    pub cache_control: Option<String>,
}

/// SQLite settings (`[database]` section).
///
/// Written either as a bare path — `database = "app.db"`, which takes the
/// defaults below — or as a table when the pragmas need tuning. The
/// defaults are what a server should have shipped with: WAL so readers do
/// not block the writer, a busy timeout so contention is a brief wait
/// rather than an error, and foreign keys actually enforced.
#[derive(Debug, Clone, Serialize)]
pub struct DatabaseConfig {
    /// Path to the SQLite file.
    pub path: PathBuf,
    /// Journal mode. `"wal"` lets readers run alongside one writer;
    /// `"delete"` (SQLite's default) serializes everything. `"keep"` leaves
    /// whatever the file already uses, which is the safe choice for a
    /// database other tools also open.
    pub journal_mode: String,
    /// Milliseconds a statement waits on a locked database before failing
    /// with `SQLITE_BUSY`.
    pub busy_timeout: u64,
    /// `synchronous` pragma. `"normal"` is the correct pairing with WAL:
    /// durable across an application crash, and only at risk from a power
    /// loss mid-checkpoint.
    pub synchronous: String,
    /// Enforce foreign-key constraints. SQLite leaves this off by default,
    /// which surprises everyone who wrote a `REFERENCES` clause.
    pub foreign_keys: bool,
    /// `cache_size` pragma, per connection. Negative values are KiB.
    pub cache_size: i64,
    /// Directory holding `NNN_name.sql` migrations. Unset looks for
    /// `migrations/` in the working directory and ignores it when absent.
    pub migrations_dir: Option<PathBuf>,
}

impl DatabaseConfig {
    /// The defaults for a given path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            journal_mode: "wal".into(),
            busy_timeout: 5_000,
            synchronous: "normal".into(),
            foreign_keys: true,
            cache_size: -2_000, // 2 MiB
            migrations_dir: None,
        }
    }

    /// The migrations directory to use, when one exists.
    pub fn migrations(&self) -> Option<PathBuf> {
        match &self.migrations_dir {
            Some(dir) => Some(dir.clone()),
            None => {
                let default = PathBuf::from("migrations");
                default.is_dir().then_some(default)
            }
        }
    }

    /// The pragma set handed to every connection.
    pub fn pragmas(&self) -> nitr_std::SqlitePragmas {
        nitr_std::SqlitePragmas {
            journal_mode: self.journal_mode.clone(),
            busy_timeout: self.busy_timeout,
            synchronous: self.synchronous.clone(),
            foreign_keys: self.foreign_keys,
            cache_size: self.cache_size,
        }
    }
}

impl<'de> Deserialize<'de> for DatabaseConfig {
    /// Accepts a bare path string or the full table.
    ///
    /// Hand-written rather than `#[serde(untagged)]` so a typo inside the
    /// table is reported as the unknown field it is, instead of "data did
    /// not match any variant".
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> std::result::Result<Self, D::Error> {
        use serde::de::{MapAccess, Visitor};

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Table {
            path: PathBuf,
            journal_mode: Option<String>,
            busy_timeout: Option<u64>,
            synchronous: Option<String>,
            foreign_keys: Option<bool>,
            cache_size: Option<i64>,
            migrations_dir: Option<PathBuf>,
        }

        struct PathOrTable;

        impl<'de> Visitor<'de> for PathOrTable {
            type Value = DatabaseConfig;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a database path or a [database] table")
            }

            fn visit_str<E: serde::de::Error>(
                self,
                path: &str,
            ) -> std::result::Result<DatabaseConfig, E> {
                Ok(DatabaseConfig::new(path))
            }

            fn visit_map<M: MapAccess<'de>>(
                self,
                map: M,
            ) -> std::result::Result<DatabaseConfig, M::Error> {
                let table = Table::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                let defaults = DatabaseConfig::new(table.path);
                Ok(DatabaseConfig {
                    journal_mode: table.journal_mode.unwrap_or(defaults.journal_mode),
                    busy_timeout: table.busy_timeout.unwrap_or(defaults.busy_timeout),
                    synchronous: table.synchronous.unwrap_or(defaults.synchronous),
                    foreign_keys: table.foreign_keys.unwrap_or(defaults.foreign_keys),
                    cache_size: table.cache_size.unwrap_or(defaults.cache_size),
                    migrations_dir: table.migrations_dir,
                    ..defaults
                })
            }
        }

        de.deserialize_any(PathOrTable)
    }
}

/// The shared `nitr.cache` (`[cache]` section).
///
/// Bounded and owned by Rust, so it is shared *data* rather than shared
/// *state*: entries are serialized on the way in, no Lua value crosses
/// between states, and the memory cannot grow past these limits.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    /// Maximum number of live entries; the least recently used is evicted
    /// past this.
    pub max_entries: usize,
    /// Maximum total size of the stored values, in bytes.
    pub max_bytes: u64,
    /// Seconds an entry lives when `set` does not say. `0` means no
    /// expiry, leaving eviction entirely to the size bounds.
    pub default_ttl: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_entries: 10_000,
            max_bytes: 32 * 1024 * 1024, // 32 MiB
            default_ttl: 300,
        }
    }
}

/// Response compression (`[compression]` section).
///
/// Off by default: compression turns a CPU-cheap server into a
/// CPU-spending one, and that should be a decision, not a surprise. One
/// line enables it. Precompressed sidecars (`app.js.br` next to `app.js`)
/// are served regardless of this section — they cost nothing at runtime.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompressionConfig {
    /// Whether responses are compressed on the fly.
    pub enabled: bool,
    /// Algorithms offered, best first. Valid names: `"br"`, `"gzip"`.
    pub algorithms: Vec<String>,
    /// Responses smaller than this are sent uncompressed: below roughly a
    /// packet, compression costs more than it saves.
    pub min_size: u64,
    /// Content types to compress. A trailing `*` matches a prefix, so
    /// `"text/*"` covers every text subtype. Already-compressed types
    /// (images, video, archives) are skipped even when listed.
    pub types: Vec<String>,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            algorithms: vec!["br".into(), "gzip".into()],
            min_size: 1024,
            types: [
                "text/*",
                "application/json",
                "application/javascript",
                "application/xml",
                "image/svg+xml",
            ]
            .map(String::from)
            .to_vec(),
        }
    }
}

/// Cross-origin resource sharing (`[cors]` section).
///
/// Enforced in Rust: a preflight never reaches a Lua state, and the policy
/// is auditable in one place instead of spread across middleware.
/// Disabled until `origins` is set.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CorsConfig {
    /// Allowed origins, or `["*"]` for any. Unset disables CORS entirely.
    pub origins: Option<Vec<String>>,
    /// Methods allowed on cross-origin requests.
    pub methods: Option<Vec<String>>,
    /// Request headers a preflight may approve.
    pub headers: Option<Vec<String>>,
    /// Response headers scripts on other origins may read.
    pub expose_headers: Option<Vec<String>>,
    /// Allow credentialed requests (cookies, `Authorization`). Cannot be
    /// combined with `origins = ["*"]`.
    pub credentials: bool,
    /// How long (seconds) a browser may cache a preflight result.
    pub max_age: Option<u64>,
}

/// Per-client-IP fixed-window rate limiting (`[rate_limit]` section).
/// Disabled by default; rejections answer 429 with a `Retry-After` header.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Whether rate limiting is enforced.
    pub enabled: bool,
    /// Allowed requests per window and client IP.
    pub requests: u32,
    /// Window length in seconds.
    pub window: u64,
    /// Key the budget by the first `X-Forwarded-For` entry instead of the
    /// peer address. Enable only behind a trusted proxy.
    pub trust_forwarded_for: bool,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            requests: 100,
            window: 60,
            trust_forwarded_for: false,
        }
    }
}

/// Lua runtime settings (`[lua]` section).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LuaConfig {
    /// Lua standard libraries loaded into every state.
    pub stdlib: Vec<String>,
    /// Per-state Lua memory limit in bytes.
    pub memory_limit: usize,
    /// Wall-clock budget per handler invocation, in milliseconds; `0`
    /// disables the limit. Enforced by an instruction-count hook (CPU-bound
    /// loops) and an outer async timeout (slow I/O).
    pub exec_timeout_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 3000)),
            handler_script: PathBuf::from("scripts/handler.lua"),
            config_script: None,
            templates_dir: None,
            database: None,
            workers: std::thread::available_parallelism().map_or(1, |n| n.get()),
            max_streams: None,
            dev_mode: false,
            std: StdConfig::default(),
            trust_request_id: false,
            limits: LimitsConfig::default(),
            rate_limit: RateLimitConfig::default(),
            fetch: FetchConfig::default(),
            shutdown: ShutdownConfig::default(),
            compression: CompressionConfig::default(),
            cors: CorsConfig::default(),
            cache: CacheConfig::default(),
            static_files: StaticConfig::default(),
            tests_dir: None,
            lua: LuaConfig::default(),
            health: HealthConfig::default(),
            log: LogConfig::default(),
            pidfile: None,
        }
    }
}

impl Default for LuaConfig {
    fn default() -> Self {
        Self {
            // `io` and `os` are deliberately excluded: they give scripts
            // ambient filesystem/process access. Opt in via `[lua] stdlib`.
            stdlib: ["math", "table", "string", "utf8", "coroutine", "package"]
                .map(String::from)
                .to_vec(),
            memory_limit: DEFAULT_MEMORY_LIMIT,
            exec_timeout_ms: DEFAULT_EXEC_TIMEOUT_MS,
        }
    }
}

impl Config {
    /// Loads the configuration from a TOML file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let data = std::fs::read_to_string(path).map_err(|err| {
            Error::Config(format!(
                "failed to read the config file {}: {err}",
                path.display()
            ))
        })?;
        toml::from_str(&data).map_err(|err| {
            Error::Config(format!(
                "failed to parse the config file {}: {err}",
                path.display()
            ))
        })
    }

    /// Applies `NITR_*` environment variable overrides on top of the current
    /// values: `NITR_LISTEN`, `NITR_HANDLER_SCRIPT`, `NITR_CONFIG_SCRIPT`,
    /// `NITR_TEMPLATES_DIR`, `NITR_DATABASE`, `NITR_WORKERS`, `NITR_MAX_STREAMS`,
    /// `NITR_DEV_MODE`, `NITR_LUA_MEMORY_LIMIT`, `NITR_LUA_EXEC_TIMEOUT_MS`,
    /// `NITR_POOL_WAIT_MS`, `NITR_SHUTDOWN_GRACE`, `NITR_COMPRESSION`,
    /// `NITR_LOG_FORMAT`, `NITR_LOG_LEVEL`, `NITR_PIDFILE`.
    pub fn apply_env(&mut self) -> Result {
        if let Some(v) = env_var("NITR_LISTEN") {
            self.listen = parse_env("NITR_LISTEN", &v)?;
        }
        if let Some(v) = env_var("NITR_HANDLER_SCRIPT") {
            self.handler_script = PathBuf::from(v);
        }
        if let Some(v) = env_var("NITR_CONFIG_SCRIPT") {
            self.config_script = Some(PathBuf::from(v));
        }
        if let Some(v) = env_var("NITR_TEMPLATES_DIR") {
            self.templates_dir = Some(PathBuf::from(v));
        }
        if let Some(v) = env_var("NITR_DATABASE") {
            // Overrides only the path; the pragmas stay as configured.
            match &mut self.database {
                Some(db) => db.path = PathBuf::from(v),
                None => self.database = Some(DatabaseConfig::new(v)),
            }
        }
        if let Some(v) = env_var("NITR_WORKERS") {
            self.workers = parse_env("NITR_WORKERS", &v)?;
        }
        if let Some(v) = env_var("NITR_MAX_STREAMS") {
            self.max_streams = Some(parse_env("NITR_MAX_STREAMS", &v)?);
        }
        if let Some(v) = env_var("NITR_DEV_MODE") {
            self.dev_mode = parse_env("NITR_DEV_MODE", &v)?;
        }
        if let Some(v) = env_var("NITR_LUA_MEMORY_LIMIT") {
            self.lua.memory_limit = parse_env("NITR_LUA_MEMORY_LIMIT", &v)?;
        }
        if let Some(v) = env_var("NITR_LUA_EXEC_TIMEOUT_MS") {
            self.lua.exec_timeout_ms = parse_env("NITR_LUA_EXEC_TIMEOUT_MS", &v)?;
        }
        if let Some(v) = env_var("NITR_POOL_WAIT_MS") {
            self.limits.pool_wait_ms = parse_env("NITR_POOL_WAIT_MS", &v)?;
        }
        if let Some(v) = env_var("NITR_SHUTDOWN_GRACE") {
            self.shutdown.grace = parse_env("NITR_SHUTDOWN_GRACE", &v)?;
        }
        if let Some(v) = env_var("NITR_COMPRESSION") {
            self.compression.enabled = parse_env("NITR_COMPRESSION", &v)?;
        }
        if let Some(v) = env_var("NITR_LOG_FORMAT") {
            self.log.format = match v.as_str() {
                "text" => LogFormat::Text,
                "json" => LogFormat::Json,
                other => {
                    return Err(Error::Config(format!(
                        "invalid NITR_LOG_FORMAT `{other}`: expected \"text\" or \"json\""
                    )));
                }
            };
        }
        if let Some(v) = env_var("NITR_LOG_LEVEL") {
            self.log.level = Some(v);
        }
        if let Some(v) = env_var("NITR_PIDFILE") {
            self.pidfile = Some(PathBuf::from(v));
        }
        Ok(())
    }

    /// Limits for the shared `nitr.cache`.
    pub fn cache_options(&self) -> nitr_std::CacheOptions {
        nitr_std::CacheOptions {
            max_entries: self.cache.max_entries.max(1),
            max_bytes: self.cache.max_bytes,
            default_ttl: self.cache.default_ttl,
        }
    }

    /// Rejects configurations that parse but cannot be honored.
    ///
    /// Called once at startup so a contradiction is a loud failure rather
    /// than a subtle runtime surprise — a browser silently ignoring a
    /// header combination is much harder to debug than a refused boot.
    pub(crate) fn validate(&self) -> Result {
        let any_origin = self
            .cors
            .origins
            .as_ref()
            .is_some_and(|o| o.iter().any(|o| o == "*"));
        if any_origin && self.cors.credentials {
            return Err(Error::Config(
                "[cors] origins = [\"*\"] cannot be combined with credentials = true: \
                 browsers reject `Access-Control-Allow-Origin: *` on a credentialed \
                 request. List the allowed origins explicitly."
                    .into(),
            ));
        }
        for name in &self.compression.algorithms {
            if !matches!(name.as_str(), "br" | "gzip") {
                return Err(Error::Config(format!(
                    "unknown [compression] algorithm `{name}`: expected \"br\" or \"gzip\""
                )));
            }
        }
        if let Some(max_streams) = self.max_streams
            && max_streams > self.workers.max(1)
        {
            return Err(Error::Config(format!(
                "max_streams = {max_streams} exceeds workers = {}: every streaming \
                 response holds a pooled Lua state, so the extra slots could never \
                 be used",
                self.workers.max(1)
            )));
        }
        // Waiting for a state longer than a handler is allowed to run means
        // the queue can only ever grow: work is admitted slower than it can
        // possibly be retired.
        if self.limits.pool_wait_ms > self.lua.exec_timeout_ms
            && self.limits.pool_wait_ms != 0
            && self.lua.exec_timeout_ms != 0
        {
            return Err(Error::Config(format!(
                "[limits] pool_wait_ms = {} exceeds [lua] exec_timeout_ms = {}: a \
                 request would wait for a state longer than any handler may run",
                self.limits.pool_wait_ms, self.lua.exec_timeout_ms
            )));
        }
        if self.health.enabled {
            for (name, path) in [
                ("liveness", &self.health.liveness),
                ("readiness", &self.health.readiness),
            ] {
                if !path.starts_with('/') {
                    return Err(Error::Config(format!(
                        "[health] {name} = `{path}` must start with `/`"
                    )));
                }
            }
            if self.health.liveness == self.health.readiness {
                return Err(Error::Config(
                    "[health] liveness and readiness must be different paths: they \
                     answer different questions (see the [health] docs)"
                        .into(),
                ));
            }
        }
        self.validate_paths()
    }

    /// Rejects paths that cannot work before any of them is opened, so a
    /// typo'd path is a startup error naming the setting, not a confusing
    /// failure minutes later.
    fn validate_paths(&self) -> Result {
        let checks: [(&str, Option<&PathBuf>, bool); 4] = [
            ("handler_script", Some(&self.handler_script), false),
            ("config_script", self.config_script.as_ref(), false),
            ("templates_dir", self.templates_dir.as_ref(), true),
            ("[static] dir", self.static_files.dir.as_ref(), true),
        ];
        for (name, path, want_dir) in checks {
            let Some(path) = path else { continue };
            if !path.exists() {
                return Err(Error::Config(format!(
                    "{name} points at {}, which does not exist",
                    path.display()
                )));
            }
            if want_dir != path.is_dir() {
                let (wanted, got) = if want_dir {
                    ("a directory", "a file")
                } else {
                    ("a file", "a directory")
                };
                return Err(Error::Config(format!(
                    "{name} must be {wanted}, but {} is {got}",
                    path.display()
                )));
            }
        }
        // The database file itself may not exist yet (SQLite creates it),
        // but its parent directory must — SQLite will not create that.
        if let Some(db) = &self.database
            && let Some(parent) = db.path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.is_dir()
        {
            return Err(Error::Config(format!(
                "the database directory {} does not exist (SQLite creates the \
                 file, not its directory)",
                parent.display()
            )));
        }
        Ok(())
    }

    /// Re-anchors the application's relative paths under `root`.
    ///
    /// Used when running from a `nitr build` bundle: the scripts, templates
    /// and static files live in the extraction directory, while the
    /// database path is deliberately left alone — it is mutable state and
    /// stays external to the artifact, resolving against the working
    /// directory as usual.
    pub fn rebase(&mut self, root: &Path) {
        let anchor = |path: &mut PathBuf| {
            if path.is_relative() {
                *path = root.join(&path);
            }
        };
        anchor(&mut self.handler_script);
        if let Some(path) = &mut self.config_script {
            anchor(path);
        }
        if let Some(path) = &mut self.templates_dir {
            anchor(path);
        }
        if let Some(path) = &mut self.static_files.dir {
            anchor(path);
        }
        if let Some(db) = &mut self.database
            && let Some(dir) = &mut db.migrations_dir
        {
            anchor(dir);
        }
        if let Some(path) = &mut self.tests_dir {
            anchor(path);
        }
    }

    /// The effective configuration after file, environment, and flag
    /// layering, rendered as TOML — the answer to "which value actually
    /// won?".
    pub fn effective_toml(&self) -> Result<String> {
        // Route through JSON first to drop the `None`s: TOML has no null,
        // and an absent key is exactly what an unset option means.
        let json = serde_json::to_value(self)
            .map_err(|err| Error::Config(format!("cannot serialize the configuration: {err}")))?;
        let json = strip_nulls(json);
        toml::to_string_pretty(&json)
            .map_err(|err| Error::Config(format!("cannot render the configuration: {err}")))
    }

    /// Resolves the configured `[std] features` list into [`Builtins`] flags.
    ///
    /// With no explicit list, the minimal default set
    /// ([`Builtins::minimal()`]: `json`, `http`, `log`, `time`,
    /// `validate`, `base64`, `path`, `url`) is enabled to keep the
    /// standard library lightweight. An explicit list is strict:
    /// unknown names or a listed feature without its required setting fail
    /// here.
    pub fn builtins(&self) -> Result<Builtins> {
        let Some(names) = &self.std.features else {
            return Ok(Builtins::minimal());
        };
        let mut builtins = Builtins::empty();
        for name in names {
            let builtin = Builtins::from_config_name(name)
                .ok_or_else(|| Error::Config(format!("unknown std feature `{name}`")))?;
            if builtin == Builtins::TEMPLATE && self.templates_dir.is_none() {
                return Err(Error::Config(
                    "std feature `template` is enabled but `templates_dir` is not set".into(),
                ));
            }
            if builtin == Builtins::DATABASE && self.database.is_none() {
                return Err(Error::Config(
                    "std feature `db` is enabled but `database` is not set".into(),
                ));
            }
            builtins |= builtin;
        }
        Ok(builtins)
    }

    /// Builds the [`RuntimeOpts`] derived from this configuration.
    pub fn runtime_opts(&self) -> Result<RuntimeOpts> {
        // Lua module loading (`require`) is confined to the directory
        // containing the handler script.
        let package_dir = self
            .handler_script
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        Ok(RuntimeOpts {
            libs: self.lua.parse_stdlib()?,
            memory_limit: self.lua.memory_limit,
            dev_mode: self.dev_mode,
            exec_timeout: match self.lua.exec_timeout_ms {
                0 => None,
                ms => Some(std::time::Duration::from_millis(ms)),
            },
            package_dir: Some(package_dir),
        })
    }
}

impl LuaConfig {
    /// Parses the stdlib names into [`mlua::StdLib`] flags.
    pub fn parse_stdlib(&self) -> Result<mlua::StdLib> {
        use mlua::StdLib;
        let mut libs = StdLib::NONE;
        for name in &self.stdlib {
            libs |= match name.as_str() {
                "coroutine" => StdLib::COROUTINE,
                "table" => StdLib::TABLE,
                "io" => StdLib::IO,
                "os" => StdLib::OS,
                "string" => StdLib::STRING,
                "utf8" => StdLib::UTF8,
                "math" => StdLib::MATH,
                "package" => StdLib::PACKAGE,
                "debug" => StdLib::DEBUG,
                _ => {
                    return Err(Error::Config(format!(
                        "unknown Lua standard library `{name}`"
                    )));
                }
            };
        }
        Ok(libs)
    }
}

/// Removes `null`s (and the entries they were) from a JSON tree, so the
/// result can render as TOML, which has no null.
fn strip_nulls(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_nulls(v)))
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(strip_nulls).collect())
        }
        other => other,
    }
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn parse_env<T: std::str::FromStr>(name: &str, value: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|err| Error::Config(format!("invalid value for {name}: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nitr_std::Builtins;

    fn write_temp_config(name: &str, content: &str) -> PathBuf {
        // `fs::write` truncates before writing, so a path two tests share is
        // a race; the counter keeps every call on its own file.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("nitr-test-{}-{id}-{name}", std::process::id()));
        std::fs::write(&path, content).expect("write temp config");
        path
    }

    /// A config whose paths exist, so `validate()` reaches the check under
    /// test instead of failing on a missing default handler script.
    fn valid_base() -> Config {
        let handler = write_temp_config("handler.lua", "-- test handler");
        Config {
            handler_script: handler,
            ..Config::default()
        }
    }

    #[test]
    fn contradictions_are_startup_errors() {
        let ok = valid_base();
        ok.validate().expect("a sane config validates");

        let mut cfg = valid_base();
        cfg.workers = 2;
        cfg.max_streams = Some(5);
        let err = cfg.validate().expect_err("max_streams > workers");
        assert!(err.to_string().contains("max_streams"), "got: {err}");

        let mut cfg = valid_base();
        cfg.limits.pool_wait_ms = 60_000;
        cfg.lua.exec_timeout_ms = 5_000;
        let err = cfg.validate().expect_err("pool_wait > exec budget");
        assert!(err.to_string().contains("pool_wait_ms"), "got: {err}");

        let mut cfg = valid_base();
        cfg.health.readiness = cfg.health.liveness.clone();
        let err = cfg.validate().expect_err("identical probe paths");
        assert!(err.to_string().contains("liveness"), "got: {err}");

        let mut cfg = valid_base();
        cfg.health.liveness = "healthz".into();
        let err = cfg.validate().expect_err("path without slash");
        assert!(err.to_string().contains("must start with"), "got: {err}");

        // Disabled health skips its checks entirely.
        let mut cfg = valid_base();
        cfg.health.enabled = false;
        cfg.health.liveness = "not-a-path".into();
        cfg.validate().expect("disabled health is not validated");
    }

    #[test]
    fn missing_paths_are_named_at_startup() {
        let mut cfg = valid_base();
        cfg.handler_script = PathBuf::from("/nonexistent/app.lua");
        let err = cfg.validate().expect_err("missing handler");
        assert!(err.to_string().contains("handler_script"), "got: {err}");

        let mut cfg = valid_base();
        cfg.templates_dir = Some(PathBuf::from("/nonexistent/templates"));
        let err = cfg.validate().expect_err("missing templates");
        assert!(err.to_string().contains("templates_dir"), "got: {err}");

        // A file where a directory belongs is as wrong as nothing at all.
        let mut cfg = valid_base();
        cfg.templates_dir = Some(cfg.handler_script.clone());
        let err = cfg.validate().expect_err("file as templates_dir");
        assert!(err.to_string().contains("directory"), "got: {err}");

        // The database file may not exist yet, but its directory must.
        let mut cfg = valid_base();
        cfg.database = Some(DatabaseConfig::new("/nonexistent/dir/app.db"));
        let err = cfg.validate().expect_err("missing db dir");
        assert!(err.to_string().contains("database directory"), "got: {err}");
    }

    #[test]
    fn unknown_keys_are_rejected_not_ignored() {
        let path = write_temp_config("typo.toml", "max_body_byte = 1\n");
        let err = Config::from_file(&path).expect_err("unknown key");
        assert!(err.to_string().contains("max_body_byte"), "got: {err}");

        // Inside a section too.
        let path = write_temp_config("typo2.toml", "[limits]\nmax_body_byte = 1\n");
        let err = Config::from_file(&path).expect_err("unknown section key");
        assert!(err.to_string().contains("max_body_byte"), "got: {err}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn effective_config_prints_and_round_trips() {
        let mut cfg = valid_base();
        cfg.log.format = LogFormat::Json;
        cfg.pidfile = Some(PathBuf::from("/run/nitr.pid"));
        let rendered = cfg.effective_toml().expect("render");
        // Nones are absent, not null.
        assert!(!rendered.contains("null"), "got: {rendered}");
        assert!(rendered.contains("format = \"json\""), "got: {rendered}");
        assert!(
            rendered.contains("pidfile = \"/run/nitr.pid\""),
            "got: {rendered}"
        );
        // The output is itself a loadable configuration.
        let path = write_temp_config("effective.toml", &rendered);
        let reparsed = Config::from_file(&path).expect("reparse");
        assert_eq!(reparsed.log.format, LogFormat::Json);
        assert_eq!(reparsed.listen, cfg.listen);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rebase_moves_app_paths_and_leaves_the_database() {
        let mut cfg = Config {
            handler_script: PathBuf::from("app.lua"),
            config_script: Some(PathBuf::from("config.lua")),
            templates_dir: Some(PathBuf::from("templates")),
            database: Some(DatabaseConfig::new("data/app.db")),
            ..Config::default()
        };
        cfg.static_files.dir = Some(PathBuf::from("public"));
        cfg.rebase(Path::new("/bundle"));
        assert_eq!(cfg.handler_script, PathBuf::from("/bundle/app.lua"));
        assert_eq!(
            cfg.config_script.as_deref(),
            Some(Path::new("/bundle/config.lua"))
        );
        assert_eq!(
            cfg.templates_dir.as_deref(),
            Some(Path::new("/bundle/templates"))
        );
        assert_eq!(
            cfg.static_files.dir.as_deref(),
            Some(Path::new("/bundle/public"))
        );
        // Mutable state stays external to the artifact.
        assert_eq!(
            cfg.database.as_ref().unwrap().path,
            PathBuf::from("data/app.db")
        );
        // Absolute paths are already anchored; rebase leaves them alone.
        let mut cfg = Config {
            handler_script: PathBuf::from("/abs/app.lua"),
            ..Config::default()
        };
        cfg.rebase(Path::new("/bundle"));
        assert_eq!(cfg.handler_script, PathBuf::from("/abs/app.lua"));
    }

    #[test]
    fn defaults_are_sane() {
        let cfg = Config::default();
        assert_eq!(cfg.listen, SocketAddr::from(([127, 0, 0, 1], 3000)));
        assert_eq!(cfg.handler_script, PathBuf::from("scripts/handler.lua"));
        assert!(cfg.workers >= 1);
        assert!(!cfg.dev_mode);
        // No explicit list enables the minimal default feature set.
        assert_eq!(cfg.builtins().expect("builtins"), Builtins::minimal());
        // io/os are opt-in.
        assert!(!cfg.lua.stdlib.iter().any(|s| s == "io" || s == "os"));
    }

    #[test]
    fn parses_a_full_config_file() {
        let path = write_temp_config(
            "full.toml",
            r#"
                listen = "127.0.0.1:8080"
                handler_script = "app/handler.lua"
                database = "app.db"
                workers = 2
                dev_mode = true
                [std]
                features = ["dbg", "json", "db"]
                [lua]
                stdlib = ["math", "string", "package"]
                memory_limit = 1048576
                exec_timeout_ms = 500
            "#,
        );
        let cfg = Config::from_file(&path).expect("parse config");
        std::fs::remove_file(&path).ok();

        assert_eq!(cfg.listen, SocketAddr::from(([127, 0, 0, 1], 8080)));
        assert!(cfg.dev_mode);
        assert_eq!(
            cfg.builtins().expect("builtins"),
            Builtins::DEBUG | Builtins::JSON | Builtins::DATABASE
        );
        let opts = cfg.runtime_opts().expect("runtime opts");
        assert_eq!(opts.memory_limit, 1048576);
        assert_eq!(
            opts.exec_timeout,
            Some(std::time::Duration::from_millis(500))
        );
        assert!(opts.dev_mode);
        // package.path confinement derives from the handler script location.
        assert_eq!(opts.package_dir.as_deref(), Some(Path::new("app")));
    }

    #[test]
    fn rejects_unknown_fields() {
        let path = write_temp_config("typo.toml", "memroy_limit = 1\n");
        let err = Config::from_file(&path).expect_err("typo must fail");
        std::fs::remove_file(&path).ok();
        assert!(err.to_string().contains("memroy_limit"));
    }

    #[test]
    fn strict_std_features_require_their_settings() {
        let mut cfg = Config {
            std: StdConfig {
                features: Some(vec!["db".into()]),
            },
            ..Config::default()
        };
        assert!(cfg.builtins().is_err());
        cfg.database = Some(DatabaseConfig::new("x.db"));
        assert_eq!(cfg.builtins().expect("builtins"), Builtins::DATABASE);

        cfg.std.features = Some(vec!["nope".into()]);
        assert!(cfg.builtins().is_err());
    }

    #[test]
    fn exec_timeout_zero_disables_the_budget() {
        let mut cfg = Config::default();
        cfg.lua.exec_timeout_ms = 0;
        assert_eq!(cfg.runtime_opts().expect("opts").exec_timeout, None);
    }

    #[test]
    fn unknown_stdlib_name_fails() {
        let mut cfg = Config::default();
        cfg.lua.stdlib.push("ffi".into());
        assert!(cfg.runtime_opts().is_err());
    }
}
