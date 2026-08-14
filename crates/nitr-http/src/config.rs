//! Server configuration (`nitr.toml`), defaults, and environment overrides.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

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
#[derive(Debug, Clone, Deserialize)]
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
    /// SQLite database file for the `conn` builtin.
    pub database: Option<PathBuf>,
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
    /// Static file serving (`[static]` section).
    #[serde(rename = "static")]
    pub static_files: StaticConfig,
    /// Directory `nitr test` discovers `*.lua` test files in.
    pub tests_dir: Option<PathBuf>,
    /// Lua runtime settings.
    pub lua: LuaConfig,
}

/// Standard library selection (`[std]` section): which built-in `nitr.*`
/// modules are exposed to scripts.
///
/// The standard library provides building blocks — scripts opt into the
/// features they need (or replace them with their own modules). Without an
/// explicit list only the minimal set is enabled to keep the footprint
/// small.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StdConfig {
    /// Enabled standard library features. Valid names: `"dbg"`, `"fetch"`,
    /// `"template"`, `"json"`, `"db"`, `"http"`, `"log"`, `"crypto"`.
    /// `None` enables the minimal default set (`json`, `http`, `log`); an
    /// explicit list is strict — unknown names or a listed feature missing
    /// its required setting (e.g. `db` without `database`) fail at startup.
    pub features: Option<Vec<String>>,
}

/// Request-size and connection limits (`[limits]` section), enforced in
/// Rust before a request reaches Lua.
#[derive(Debug, Clone, Deserialize)]
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
        }
    }
}

/// Graceful-shutdown timing (`[shutdown]` section).
///
/// On `SIGTERM`/`SIGINT` the server stops accepting, lets in-flight
/// requests finish, and only then exits. These bound how long it waits.
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
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
}

impl Default for FetchConfig {
    fn default() -> Self {
        let defaults = nitr_std::FetchOptions::default();
        Self {
            allowed_hosts: defaults.allowed_hosts,
            allow_private_networks: defaults.allow_private_networks,
            max_response_bytes: defaults.max_response_bytes,
            max_concurrent: defaults.max_concurrent,
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
        }
    }
}

/// Static file serving (`[static]` section): requests under `mount` are
/// served from `dir` entirely in Rust, before any Lua dispatch. Scripts
/// can add further mounts with `app:static(mount, dir, opts?)`.
#[derive(Debug, Clone, Default, Deserialize)]
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

/// Per-client-IP fixed-window rate limiting (`[rate_limit]` section).
/// Disabled by default; rejections answer 429 with a `Retry-After` header.
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
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
            static_files: StaticConfig::default(),
            tests_dir: None,
            lua: LuaConfig::default(),
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
    /// `NITR_POOL_WAIT_MS`, `NITR_SHUTDOWN_GRACE`.
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
            self.database = Some(PathBuf::from(v));
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
        Ok(())
    }

    /// Resolves the configured `[std] features` list into [`Builtins`] flags.
    ///
    /// With no explicit list, the minimal default set
    /// ([`Builtins::minimal()`]: `json`, `http`, `log`) is enabled to keep
    /// the standard library lightweight. An explicit list is strict:
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
                    )))
                }
            };
        }
        Ok(libs)
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
        let path = std::env::temp_dir().join(format!("nitr-test-{}-{name}", std::process::id()));
        std::fs::write(&path, content).expect("write temp config");
        path
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
        cfg.database = Some(PathBuf::from("x.db"));
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
