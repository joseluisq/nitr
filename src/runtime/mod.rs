use mlua::{
    FromLua, Function, HookTriggers, IntoLua, IntoLuaMulti, Lua, LuaOptions, LuaSerdeExt as _,
    RegistryKey, StdLib, Table, Thread, Value, VmState,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::lua::{db, fetch, json, template, utils, Builtins};

pub(crate) mod pool;

pub use pool::{RuntimeGuard, RuntimePool};

const MEMORY_LIMIT: usize = 8 * 1024 * 1024; // 8 MiB

/// Default wall-clock budget per handler invocation.
const EXEC_TIMEOUT: Duration = Duration::from_secs(30);

/// How often (in Lua VM instructions) the execution-deadline hook runs.
const HOOK_INSTRUCTION_INTERVAL: u32 = 4000;

/// Extra wall-clock grace given to the outer async timeout so the
/// instruction hook (with its precise error message) fires first for
/// CPU-bound overruns.
const EXEC_TIMEOUT_GRACE: Duration = Duration::from_millis(100);

/// The Lua runtime that provides an interface to execute Lua scripts and manage Lua state.
/// It allows for registering global functions, configuration scripts, and HTTP handlers.
#[derive(Debug)]
pub struct Runtime {
    lua: Lua,
    cfg: Option<Table>,
    http_fn: Option<Function>,
    http_fn_key: Option<RegistryKey>,
    http_fn_path: Option<PathBuf>,
    /// Cached handler coroutine, reset and reused across requests to avoid
    /// per-request thread allocation and hook installation.
    thread: Option<Thread>,
    /// Execution deadline for the instruction hook, in nanoseconds since
    /// `epoch`. Stored atomically so the hook closure (installed once per
    /// thread) reads the current request's deadline without locking.
    deadline: Arc<AtomicU64>,
    epoch: Instant,
    opts: RuntimeOpts,
}

/// Options for configuring the Lua runtime.
#[derive(Debug)]
pub struct RuntimeOpts {
    /// Lua standard libraries to load.
    pub libs: mlua::StdLib,
    /// Lua memory limit in bytes.
    pub memory_limit: usize,
    /// Development mode: reload the HTTP handler script before each call and
    /// include error details (Lua tracebacks) in error responses.
    pub dev_mode: bool,
    /// Execution budget per handler invocation, enforced by an
    /// instruction-count hook (CPU-bound loops) and an outer async timeout
    /// (slow I/O). `None` disables both.
    pub exec_timeout: Option<Duration>,
    /// Directory `require` is confined to: `package.path` is pinned to it
    /// and `package.cpath` is cleared (no native modules). `None` leaves the
    /// Lua defaults untouched.
    pub package_dir: Option<PathBuf>,
}

impl Runtime {
    /// It creates a new Lua runtime with default options.
    ///
    /// Such as some **built-in** libraries loaded and a default memory limit.
    pub fn new() -> Result<Self> {
        // `io` and `os` are deliberately excluded from the defaults: they
        // give scripts ambient filesystem/process access. Opt in via
        // `RuntimeOpts::libs` when needed.
        Runtime::new_with(RuntimeOpts {
            libs: StdLib::NONE
                | StdLib::MATH
                | StdLib::TABLE
                | StdLib::STRING
                | StdLib::PACKAGE
                | StdLib::UTF8
                | StdLib::COROUTINE,
            memory_limit: MEMORY_LIMIT,
            dev_mode: false,
            exec_timeout: Some(EXEC_TIMEOUT),
            package_dir: None,
        })
    }

    /// It creates a new Lua runtime with specified options.
    ///
    /// For example, it allows for customizing the Lua standard libraries to load
    /// like `io`, `math`, `os`, etc as well as the memory limits.
    pub fn new_with(opts: RuntimeOpts) -> Result<Self> {
        let lua = Lua::new_with(opts.libs, LuaOptions::default())?;
        lua.set_memory_limit(opts.memory_limit)?;

        // Confine `require` to the configured directory and forbid loading
        // native modules.
        if opts.libs.contains(StdLib::PACKAGE) {
            if let Some(dir) = &opts.package_dir {
                let dir = dir.to_string_lossy();
                let package: Table = lua.globals().get("package")?;
                package.set("path", format!("{dir}/?.lua;{dir}/?/init.lua"))?;
                package.set("cpath", "")?;
            }
        }

        Ok(Self {
            lua,
            cfg: None,
            http_fn: None,
            http_fn_key: None,
            http_fn_path: None,
            thread: None,
            deadline: Arc::new(AtomicU64::new(u64::MAX)),
            epoch: Instant::now(),
            opts,
        })
    }

    /// It sets the Lua global functions for the specified **built-in** libraries
    /// like `dbg`, `fetch`, `template`, etc to be accessible in the Lua scripts.
    ///
    /// Builtins that need a setting from the [`Config`] (`template` needs
    /// `templates_dir`, `db` needs `database`) are skipped with a warning
    /// when that setting is absent; [`Config::builtins()`] rejects such
    /// combinations upfront when the builtins were listed explicitly.
    ///
    /// For setting custom libraries, use the singular [`set_global()`](Self::set_global) method.
    pub fn register_builtins(&self, builtins: Builtins, cfg: &Config) -> Result {
        let globals = self.lua.globals();
        for builtin in builtins.iter() {
            let Some(name) = builtin.global_name() else {
                continue;
            };
            match builtin {
                Builtins::DEBUG => globals.set(name, utils::create_debug_fn(&self.lua)?)?,
                Builtins::FETCH => globals.set(name, fetch::create_fetch_fn(&self.lua)?)?,
                Builtins::TEMPLATE => match &cfg.templates_dir {
                    Some(dir) => {
                        globals.set(name, template::create_template_fn(&self.lua, dir)?)?
                    }
                    None => {
                        tracing::warn!(
                            "skipping builtin `template`: `templates_dir` is not configured"
                        );
                    }
                },
                Builtins::JSON => globals.set(name, json::create_json_fn(&self.lua)?)?,
                Builtins::DATABASE => match &cfg.database {
                    Some(path) => globals.set(name, db::create_database_fn(&self.lua, path)?)?,
                    None => {
                        tracing::warn!("skipping builtin `db`: `database` is not configured");
                    }
                },
                _ => continue,
            };
        }
        Ok(())
    }

    /// It sets a custom global Lua variable with the specified key and value.
    ///
    /// For setting **built-in** globals, use the [`register_builtins()`](Self::register_builtins) method.
    pub fn set_global<V: IntoLua>(&self, key: impl IntoLua, value: V) -> Result {
        self.lua.globals().set(key, value)?;
        Ok(())
    }

    /// It sets the Lua configuration function that will be called at server startup.
    ///
    /// It loads the Lua script from the path and evaluates it to allocate the function,
    /// then it's immediately invoked with the provided arguments if any.
    /// The Lua table containing the configuration fields can be accessed later
    /// using the [`cfg()`](Self::cfg) method.
    pub async fn register_cfg_fn(&mut self, cfg_src: &Path, args: impl IntoLuaMulti) -> Result {
        let data = std::fs::read(cfg_src).map_err(|err| {
            Error::Script(format!(
                "failed to read the Lua configuration file {}: {err}",
                cfg_src.display()
            ))
        })?;

        // Create config handler and call it
        let key = self.lua.load(data).eval::<RegistryKey>()?;
        let cfg_fn = self.lua.registry_value::<Function>(&key)?;
        let cfg = cfg_fn.call_async::<Table>(args).await?;

        self.cfg = Some(cfg);
        Ok(())
    }

    /// It sets the Lua HTTP handler function that will be called on every HTTP request.
    ///
    /// It loads the Lua script from the path and evaluates it to allocate the function,
    /// but it's not invoked immediately. It will be called on every request.
    pub fn register_http_fn(&mut self, http_src: &Path) -> Result {
        let meta = std::fs::metadata(http_src).map_err(|err| {
            Error::Script(format!(
                "failed to read HTTP handler file metadata for {}: {err}",
                http_src.display()
            ))
        })?;

        if meta.is_file() {
            self.http_fn_path = Some(http_src.to_owned());
        } else {
            return Err(Error::Script(format!(
                "HTTP handler path {} is not a regular file",
                http_src.display()
            )));
        }

        let data = std::fs::read(http_src).map_err(|err| {
            Error::Script(format!(
                "failed to read the Lua HTTP handler file {}: {err}",
                http_src.display()
            ))
        })?;

        let key = self.lua.load(data).eval::<RegistryKey>()?;
        let http_fn = self.lua.registry_value::<Function>(&key)?;
        self.http_fn_key = Some(key);

        self.http_fn = Some(http_fn);
        Ok(())
    }

    /// The underlying Lua state, for advanced customization such as
    /// registering custom globals or modules.
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// Get a global Lua variable by key.
    ///
    /// Note that this function can also access a **built-in** global.
    pub fn get_global<V: FromLua>(&mut self, key: impl IntoLua) -> Result<V> {
        let value = self.lua.globals().get::<V>(key)?;
        Ok(value)
    }

    /// The Lua configuration table that is returned after the script handler is invoked.
    pub fn cfg(&self) -> Option<&Table> {
        self.cfg.as_ref()
    }

    /// Serializes the configuration table into a plain-data snapshot that can
    /// be injected into other runtimes with
    /// [`set_cfg_snapshot()`](Self::set_cfg_snapshot).
    ///
    /// Returns `None` when no configuration script has been registered.
    pub fn cfg_snapshot(&self) -> Result<Option<serde_json::Value>> {
        let Some(cfg) = &self.cfg else {
            return Ok(None);
        };
        let snapshot = serde_json::to_value(cfg).map_err(|err| {
            Error::Config(format!(
                "the configuration script must return plain data \
                 (tables, strings, numbers, booleans): {err}"
            ))
        })?;
        Ok(Some(snapshot))
    }

    /// Injects a configuration snapshot produced by
    /// [`cfg_snapshot()`](Self::cfg_snapshot) as this runtime's
    /// configuration table.
    pub fn set_cfg_snapshot(&mut self, snapshot: &serde_json::Value) -> Result {
        match self.lua.to_value(snapshot)? {
            Value::Table(table) => {
                self.cfg = Some(table);
                Ok(())
            }
            _ => Err(Error::Config(
                "the configuration snapshot must be a table".into(),
            )),
        }
    }

    /// The Lua HTTP handler function that will be called for each HTTP request.
    pub fn http_fn(&self) -> Option<&Function> {
        self.http_fn.as_ref()
    }

    /// Returns the cached handler coroutine reset to `http_fn`, creating it
    /// (and installing the execution-deadline hook once) when necessary.
    ///
    /// The handler runs in its own coroutine so the hook can be attached to
    /// it (Lua hooks are per thread; a hook on the main state would never
    /// fire inside the coroutine).
    fn handler_thread(&mut self, http_fn: Function) -> Result<Thread> {
        if let Some(thread) = self.thread.take() {
            if thread.reset(http_fn.clone()).is_ok() {
                return Ok(thread);
            }
        }
        let thread = self.lua.create_thread(http_fn)?;
        if self.opts.exec_timeout.is_some() {
            // Instruction-count hook: the only mechanism that can stop a
            // CPU-bound loop (`while true do end` never reaches an await
            // point, blocking both the async timeout and the executor).
            let deadline = self.deadline.clone();
            let epoch = self.epoch;
            thread.set_hook(
                HookTriggers::new().every_nth_instruction(HOOK_INSTRUCTION_INTERVAL),
                move |_, _| {
                    if epoch.elapsed().as_nanos() as u64 > deadline.load(Ordering::Relaxed) {
                        return Err(mlua::Error::RuntimeError(
                            "handler execution exceeded its time budget".into(),
                        ));
                    }
                    Ok(VmState::Continue)
                },
            )?;
        }
        Ok(thread)
    }

    /// Calls the registered HTTP handler with the given request under the
    /// configured execution budget ([`RuntimeOpts::exec_timeout`]): an
    /// instruction-count hook stops CPU-bound overruns and an outer async
    /// timeout stops slow I/O ([`Error::Timeout`]).
    pub async fn call_handler(&mut self, req: impl IntoLua) -> Result<Table> {
        let http_fn = self
            .http_fn
            .clone()
            .ok_or_else(|| Error::Script("no HTTP handler has been registered".into()))?;

        let thread = self.handler_thread(http_fn)?;
        let args = (self.cfg.as_ref(), req);
        let result = match self.opts.exec_timeout {
            Some(timeout) => {
                self.deadline.store(
                    (self.epoch.elapsed() + timeout).as_nanos() as u64,
                    Ordering::Relaxed,
                );
                // The async timeout covers the disjoint failure mode: time
                // spent suspended in async I/O, where no Lua instructions
                // execute and the hook cannot fire.
                tokio::time::timeout(
                    timeout + EXEC_TIMEOUT_GRACE,
                    thread.clone().into_async::<Table>(args)?,
                )
                .await
                .map_err(|_| Error::Timeout)?
            }
            None => thread.clone().into_async::<Table>(args)?.await,
        };
        // Keep the coroutine for the next request (reset() also recovers
        // errored threads on Lua 5.4).
        self.thread = Some(thread);
        Ok(result?)
    }

    /// Whether this runtime operates in development mode.
    pub fn dev_mode(&self) -> bool {
        self.opts.dev_mode
    }

    /// Reloads the Lua HTTP handler function from the file specified in `http_fn_path`.
    pub fn http_fn_reload(&mut self) -> Result<()> {
        // TODO: group all those fields in a struct
        if !self.opts.dev_mode
            || self.http_fn.is_none()
            || self.http_fn_key.is_none()
            || self.http_fn_path.is_none()
        {
            return Ok(());
        }

        let http_fn_path = self.http_fn_path.as_ref().unwrap();
        tracing::debug!("reloading http handler from {}", http_fn_path.display());

        let data = std::fs::read(http_fn_path).map_err(|err| {
            Error::Script(format!(
                "failed to read the Lua HTTP handler file {}: {err}",
                http_fn_path.display()
            ))
        })?;

        let http_fn = self.lua.load(data).eval::<Function>()?;
        let mut existing_key = self.http_fn_key.take().unwrap();
        self.lua
            .replace_registry_value(&mut existing_key, http_fn)?;
        let http_fn = self.lua.registry_value::<Function>(&existing_key)?;
        self.http_fn = Some(http_fn);
        self.http_fn_key = Some(existing_key);

        Ok(())
    }
}
