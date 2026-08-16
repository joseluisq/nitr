use mlua::{
    FromLuaMulti, Function, HookTriggers, IntoLuaMulti, Lua, LuaOptions, LuaSerdeExt as _,
    RegistryKey, StdLib, Table, Thread, Value, VmState,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

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
/// It allows for registering Rust extension modules on the `nitr` namespace,
/// running a configuration script, and calling Lua functions under the
/// state's execution budget.
#[derive(Debug)]
pub struct Runtime {
    lua: Lua,
    cfg: Option<Table>,
    /// Cached handler coroutine, reset and reused across requests to avoid
    /// per-request thread allocation and hook installation.
    thread: Option<Thread>,
    /// Execution deadline for the instruction hook, in nanoseconds since
    /// `epoch`. Stored atomically so the hook closure (installed once for
    /// the whole state) reads the current request's deadline without
    /// locking.
    deadline: Arc<AtomicU64>,
    epoch: Instant,
    /// Set when a failure leaves this state unfit for reuse (memory limit
    /// hit, panic). The pool rebuilds a poisoned state instead of handing
    /// it to the next request.
    poisoned: bool,
    opts: RuntimeOpts,
}

/// Grants additional execution budget to a runtime while it produces a
/// streaming response: each chunk handed to the client resets the
/// instruction-hook deadline, so the budget applies per chunk-production
/// slice instead of to the total stream lifetime.
#[derive(Debug, Clone)]
pub struct DeadlineHandle {
    deadline: Arc<AtomicU64>,
    epoch: Instant,
    budget: Option<Duration>,
}

impl DeadlineHandle {
    /// Grants another full execution budget from now. A no-op when the
    /// runtime has no execution timeout configured.
    pub fn extend(&self) {
        if let Some(budget) = self.budget {
            self.deadline.store(
                (self.epoch.elapsed() + budget).as_nanos() as u64,
                Ordering::Relaxed,
            );
        }
    }
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
        if opts.libs.contains(StdLib::PACKAGE)
            && let Some(dir) = &opts.package_dir
        {
            let dir = dir.to_string_lossy();
            let package: Table = lua.globals().get("package")?;
            package.set("path", format!("{dir}/?.lua;{dir}/?/init.lua"))?;
            package.set("cpath", "")?;
        }

        let deadline = Arc::new(AtomicU64::new(u64::MAX));
        let epoch = Instant::now();

        // Instruction-count hook: the only mechanism that can stop a
        // CPU-bound loop (`while true do end` never reaches an await point,
        // blocking both the async timeout and the executor).
        //
        // It is installed *globally* rather than per coroutine: Lua 5.4
        // propagates a state's hook to threads created from it, so a
        // `coroutine.create` inside a handler inherits the budget instead of
        // escaping it.
        if opts.exec_timeout.is_some() {
            let deadline = deadline.clone();
            lua.set_global_hook(
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

        Ok(Self {
            lua,
            cfg: None,
            thread: None,
            deadline,
            epoch,
            poisoned: false,
            opts,
        })
    }

    /// Whether a failure has left this state unfit for reuse (a memory
    /// limit hit or a caught panic). The pool rebuilds a poisoned state
    /// rather than handing it to the next request.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Marks this state unfit for reuse. Used by the HTTP layer when a
    /// panic is caught while the state is checked out.
    pub fn poison(&mut self) {
        self.poisoned = true;
    }

    /// Registers a Rust extension module: the closure runs now (and its
    /// result is a table by Lua module convention), mounted at
    /// `nitr.<name>`. Fails when the name is already taken, so extensions
    /// cannot shadow builtins or each other.
    ///
    /// This is the embedding-side extension point; HTTP applications use
    /// `ServerBuilder::module()` in the `nitr-http` crate, which applies
    /// the closure to every pooled state.
    pub fn register_module<F>(&self, name: &str, f: F) -> Result
    where
        F: Fn(&Lua) -> mlua::Result<Table>,
    {
        let value = f(&self.lua)?;
        crate::ns::mount(&self.lua, name, value)
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

    /// The underlying Lua state, for advanced customization beyond
    /// [`register_module()`](Self::register_module).
    pub fn lua(&self) -> &Lua {
        &self.lua
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

    /// Returns the cached handler coroutine reset to the given function,
    /// creating it when necessary.
    ///
    /// The handler runs in its own coroutine; the execution-deadline hook is
    /// installed globally at construction and inherited by every thread,
    /// including coroutines the script creates itself.
    fn handler_thread(&mut self, http_fn: Function) -> Result<Thread> {
        if let Some(thread) = self.thread.take()
            && thread.reset(http_fn.clone()).is_ok()
        {
            return Ok(thread);
        }
        Ok(self.lua.create_thread(http_fn)?)
    }

    /// Calls a Lua function under the configured execution budget
    /// ([`RuntimeOpts::exec_timeout`]): an instruction-count hook stops
    /// CPU-bound overruns and an outer async timeout stops slow I/O
    /// ([`Error::Timeout`]). The function runs on this runtime's cached
    /// coroutine so the instruction-count hook applies.
    pub async fn call_function<R: FromLuaMulti>(
        &mut self,
        f: Function,
        args: impl IntoLuaMulti,
    ) -> Result<R> {
        let thread = self.handler_thread(f)?;
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
                    thread.clone().into_async::<R>(args)?,
                )
                .await
                .map_err(|_| Error::Timeout)?
            }
            None => thread.clone().into_async::<R>(args)?.await,
        };
        // Keep the coroutine for the next request (reset() also recovers
        // errored threads on Lua 5.4).
        self.thread = Some(thread);
        self.classify(result)
    }

    /// Calls a Lua function under the instruction-hook deadline only — no
    /// outer async timeout — for long-lived streaming calls. The caller is
    /// expected to keep granting budget via [`DeadlineHandle::extend()`] as
    /// chunks are delivered; time suspended in async I/O (e.g. waiting for a
    /// slow client) is deliberately unbounded.
    pub async fn call_function_streaming<R: FromLuaMulti>(
        &mut self,
        f: Function,
        args: impl IntoLuaMulti,
    ) -> Result<R> {
        let thread = self.handler_thread(f)?;
        if let Some(timeout) = self.opts.exec_timeout {
            self.deadline.store(
                (self.epoch.elapsed() + timeout).as_nanos() as u64,
                Ordering::Relaxed,
            );
        }
        let result = thread.clone().into_async::<R>(args)?.await;
        self.thread = Some(thread);
        self.classify(result)
    }

    /// Converts a call outcome into a [`Result`], marking the state
    /// poisoned when the failure is one it cannot cleanly recover from.
    fn classify<R>(&mut self, result: mlua::Result<R>) -> Result<R> {
        match result {
            Ok(value) => Ok(value),
            Err(err) => {
                let err = Error::from(err);
                if err.poisons_state() {
                    self.poisoned = true;
                }
                Err(err)
            }
        }
    }

    /// A handle for extending this runtime's execution deadline from
    /// long-lived calls (see
    /// [`call_function_streaming()`](Self::call_function_streaming)).
    pub fn deadline_handle(&self) -> DeadlineHandle {
        DeadlineHandle {
            deadline: self.deadline.clone(),
            epoch: self.epoch,
            budget: self.opts.exec_timeout,
        }
    }

    /// Whether this runtime operates in development mode.
    pub fn dev_mode(&self) -> bool {
        self.opts.dev_mode
    }

    /// Loads and evaluates a Lua script file, returning the resulting value.
    ///
    /// This does not interpret the result; callers decide what the script is
    /// expected to return (e.g. a handler function or an application object).
    pub fn eval_script(&self, path: &Path) -> Result<Value> {
        let data = std::fs::read(path).map_err(|err| {
            Error::Script(format!(
                "failed to read the Lua script {}: {err}",
                path.display()
            ))
        })?;
        Ok(self.lua.load(data).eval::<Value>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp_script(name: &str, content: &str) -> PathBuf {
        // `fs::write` truncates before writing, so a path two tests share is
        // a race; the counter keeps every call on its own file.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("nitr-rt-test-{}-{id}-{name}", std::process::id()));
        std::fs::write(&path, content).expect("write temp script");
        path
    }

    fn test_runtime(exec_timeout: Option<Duration>) -> Runtime {
        Runtime::new_with(RuntimeOpts {
            libs: StdLib::MATH | StdLib::TABLE | StdLib::STRING,
            memory_limit: 8 * 1024 * 1024,
            dev_mode: false,
            exec_timeout,
            package_dir: None,
        })
        .expect("runtime")
    }

    fn eval_function(rt: &Runtime, src: &str) -> Function {
        rt.lua().load(src).eval().expect("eval handler function")
    }

    #[tokio::test]
    async fn function_calls_round_trip() {
        let mut rt = test_runtime(Some(Duration::from_secs(5)));
        let f = eval_function(
            &rt,
            "return function(req) return { status = 200, body = req } end",
        );

        // The cached coroutine must keep working across calls.
        for _ in 0..3 {
            let resp = rt
                .call_function::<Table>(f.clone(), "ping")
                .await
                .expect("call function");
            assert_eq!(resp.get::<String>("body").expect("body"), "ping");
        }
    }

    #[tokio::test]
    async fn cpu_bound_loops_hit_the_instruction_hook() {
        let mut rt = test_runtime(Some(Duration::from_millis(100)));
        let looping = eval_function(&rt, "return function() while true do end end");

        let err = rt
            .call_function::<Table>(looping, Value::Nil)
            .await
            .expect_err("must time out");
        assert!(err.to_string().contains("time budget"), "got: {err}");

        // The state must survive and serve the next call after a reset.
        let ok = eval_function(&rt, "return function() return { body = 'alive' } end");
        let resp = rt
            .call_function::<Table>(ok, Value::Nil)
            .await
            .expect("recovered");
        assert_eq!(resp.get::<String>("body").expect("body"), "alive");
    }

    #[test]
    fn modules_mount_under_nitr_and_reject_collisions() {
        let rt = test_runtime(None);
        rt.register_module("greet", |lua| {
            let t = lua.create_table()?;
            t.set(
                "hello",
                lua.create_function(|_, name: String| Ok(format!("hi {name}")))?,
            )?;
            Ok(t)
        })
        .expect("register module");

        let out: String = rt
            .lua()
            .load("return nitr.greet.hello('nitr')")
            .eval()
            .expect("call module");
        assert_eq!(out, "hi nitr");

        // A second mount under the same name must fail loudly.
        let err = rt
            .register_module("greet", |lua| lua.create_table())
            .expect_err("collision");
        assert!(err.to_string().contains("already exists"), "got: {err}");
    }

    #[tokio::test]
    async fn config_snapshot_round_trips() {
        let cfg_script = write_temp_script(
            "cfg.lua",
            "function() return { greeting = 'hi', nested = { n = 7 } } end",
        );
        let mut source = test_runtime(None);
        source
            .register_cfg_fn(&cfg_script, Value::Nil)
            .await
            .expect("run config script");
        std::fs::remove_file(&cfg_script).ok();

        let snapshot = source
            .cfg_snapshot()
            .expect("snapshot")
            .expect("config present");
        let mut target = test_runtime(None);
        target.set_cfg_snapshot(&snapshot).expect("inject snapshot");
        let cfg = target.cfg().expect("cfg table");
        assert_eq!(cfg.get::<String>("greeting").expect("greeting"), "hi");
        let nested: Table = cfg.get("nested").expect("nested");
        assert_eq!(nested.get::<i64>("n").expect("n"), 7);
    }
}
