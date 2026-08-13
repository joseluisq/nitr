//! The HTTP server and its builder: the main entrypoint for consuming Nitr.

use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use mlua::AnyUserData;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::lua::Builtins;
use crate::runtime::{Runtime, RuntimePool};
use crate::service::Svc;

/// How long to read request headers before giving up on a connection.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for in-flight requests on shutdown.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// A closure that customizes each pooled Lua state (custom globals/modules).
type SetupFn = Box<dyn Fn(&mlua::Lua) -> mlua::Result<()> + Send + Sync>;

/// The Nitr HTTP server: a pool of Lua runtimes behind a shared listener.
///
/// Built via [`Server::builder()`]; run via [`serve()`](Self::serve).
pub struct Server {
    cfg: Config,
    pool: Arc<RuntimePool>,
}

/// Builder for [`Server`].
///
/// Individual setters override values from [`config()`](Self::config).
#[derive(Default)]
pub struct ServerBuilder {
    cfg: Config,
    builtins: Option<Builtins>,
    setup_fns: Vec<SetupFn>,
}

impl Server {
    /// Creates a [`ServerBuilder`] with default configuration.
    pub fn builder() -> ServerBuilder {
        ServerBuilder::default()
    }

    /// The pool of Lua runtimes serving requests.
    pub fn pool(&self) -> &Arc<RuntimePool> {
        &self.pool
    }

    /// Serves until `ctrl-c`, then shuts down gracefully.
    pub async fn serve(self) -> Result {
        self.serve_with_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
    }

    /// Serves until the given future resolves, then shuts down gracefully:
    /// the listener stops accepting and in-flight requests get a grace
    /// period to complete.
    pub async fn serve_with_shutdown(self, shutdown: impl Future<Output = ()>) -> Result {
        let listener = TcpListener::bind(self.cfg.listen).await.map_err(|err| {
            Error::Config(format!("unable to listen on {}: {err}", self.cfg.listen))
        })?;
        tracing::info!(
            "listening on http://{} with {} Lua state(s)",
            self.cfg.listen,
            self.pool.size()
        );

        let graceful = GracefulShutdown::new();
        let mut shutdown = std::pin::pin!(shutdown);

        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, peer_addr) = match accepted {
                        Ok(x) => x,
                        Err(err) => {
                            tracing::error!("failed to accept connection: {err}");
                            continue;
                        }
                    };

                    // Small responses must not wait on Nagle's algorithm.
                    let _ = stream.set_nodelay(true);

                    let svc = Svc::new(self.pool.clone(), peer_addr);
                    let conn = http1::Builder::new()
                        .timer(TokioTimer::new())
                        .header_read_timeout(HEADER_READ_TIMEOUT)
                        .serve_connection(TokioIo::new(stream), svc);
                    let conn = graceful.watch(conn);
                    tokio::spawn(async move {
                        if let Err(err) = conn.await {
                            tracing::error!("error serving connection: {err}");
                        }
                    });
                }
                _ = &mut shutdown => break,
            }
        }

        tracing::info!("shutting down, waiting for in-flight requests");
        tokio::select! {
            _ = graceful.shutdown() => {}
            _ = tokio::time::sleep(SHUTDOWN_GRACE) => {
                tracing::warn!(
                    "graceful shutdown timed out after {SHUTDOWN_GRACE:?}, aborting connections"
                );
            }
        }
        Ok(())
    }
}

impl ServerBuilder {
    /// Bulk-applies a loaded [`Config`] (e.g. from `nitr.toml`).
    /// Setters called afterwards override it.
    pub fn config(mut self, cfg: Config) -> Self {
        self.cfg = cfg;
        self
    }

    /// Address the server binds to.
    pub fn listen(mut self, addr: std::net::SocketAddr) -> Self {
        self.cfg.listen = addr;
        self
    }

    /// Lua script executed once per request.
    pub fn handler_script(mut self, path: impl Into<PathBuf>) -> Self {
        self.cfg.handler_script = path.into();
        self
    }

    /// Lua script executed exactly once at startup; its returned table is
    /// passed to the handler on every request.
    pub fn config_script(mut self, path: impl Into<PathBuf>) -> Self {
        self.cfg.config_script = Some(path.into());
        self
    }

    /// Directory for the `template` builtin.
    pub fn templates_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.cfg.templates_dir = Some(path.into());
        self
    }

    /// SQLite database file for the `conn` builtin.
    pub fn database(mut self, path: impl Into<PathBuf>) -> Self {
        self.cfg.database = Some(path.into());
        self
    }

    /// Built-in globals to expose; overrides the configuration file list.
    pub fn builtins(mut self, builtins: Builtins) -> Self {
        self.builtins = Some(builtins);
        self
    }

    /// Number of pooled Lua states (max concurrently executing handlers).
    pub fn workers(mut self, n: usize) -> Self {
        self.cfg.workers = n;
        self
    }

    /// Development mode: hot-reload the handler script on change.
    pub fn dev_mode(mut self, on: bool) -> Self {
        self.cfg.dev_mode = on;
        self
    }

    /// Registers a closure that customizes each pooled Lua state — the
    /// extension point for custom globals/modules. It runs once per state,
    /// before the configuration script and handler are loaded.
    pub fn setup<F>(mut self, f: F) -> Self
    where
        F: Fn(&mlua::Lua) -> mlua::Result<()> + Send + Sync + 'static,
    {
        self.setup_fns.push(Box::new(f));
        self
    }

    /// Builds the server: creates the runtime pool, runs the configuration
    /// script exactly once, snapshots its result into every state, and
    /// compiles the handler. Fails fast on any configuration or script error.
    pub async fn build(self) -> Result<Server> {
        let cfg = self.cfg;
        let builtins = match self.builtins {
            Some(b) => b,
            None => cfg.builtins()?,
        };
        let workers = cfg.workers.max(1);

        // Bootstrap state: runs the configuration script exactly once.
        let mut bootstrap = new_runtime(&cfg, builtins, &self.setup_fns)?;
        let snapshot = match &cfg.config_script {
            Some(conf_src) => {
                // Pass the database connection to the config script when available.
                let db_name = Builtins::DATABASE
                    .global_name()
                    .expect("DATABASE is a single builtin flag");
                let db = bootstrap.get_global::<Option<AnyUserData>>(db_name)?;
                bootstrap.register_cfg_fn(conf_src, db).await?;
                bootstrap.cfg_snapshot()?
            }
            None => None,
        };
        bootstrap.register_http_fn(&cfg.handler_script)?;

        // Remaining states: inject the snapshot instead of re-running the
        // configuration script, so its side effects happen exactly once.
        let mut runtimes = Vec::with_capacity(workers);
        runtimes.push(bootstrap);
        for _ in 1..workers {
            let mut rt = new_runtime(&cfg, builtins, &self.setup_fns)?;
            if let Some(snapshot) = &snapshot {
                rt.set_cfg_snapshot(snapshot)?;
            }
            rt.register_http_fn(&cfg.handler_script)?;
            runtimes.push(rt);
        }

        Ok(Server {
            cfg,
            pool: Arc::new(RuntimePool::new(runtimes)),
        })
    }
}

fn new_runtime(cfg: &Config, builtins: Builtins, setup_fns: &[SetupFn]) -> Result<Runtime> {
    let rt = Runtime::new_with(cfg.runtime_opts()?)?;
    rt.register_builtins(builtins, cfg)?;
    for setup in setup_fns {
        setup(rt.lua())?;
    }
    Ok(rt)
}
