use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use clap::{Parser, Subcommand};

use nitr::{BuiltinsEnv, Config, Runtime, Server};

mod bundle;
mod diag;

const DEFAULT_CONFIG_FILE: &str = "nitr.toml";

/// The Nitr server: drop a binary and a few Lua files onto a machine and
/// you have a complete small HTTP application.
#[derive(Parser)]
#[command(
    name = "nitr",
    disable_version_flag = true,
    after_help = "Signals:\n  SIGHUP           Zero-downtime reload: rebuilds the Lua runtime pool"
)]
struct Cli {
    /// Print the version and exit.
    #[arg(short = 'v', long = "version", global = true)]
    version: bool,
    /// Path to the TOML config file (default: ./nitr.toml).
    #[arg(short = 'c', long = "config", global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Enable development mode (hot reload).
    #[arg(long, global = true)]
    dev: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the server (the default when no command is given).
    Run,
    /// Start the server in development mode (hot reload).
    Dev,
    /// Load the configuration and scripts, then exit.
    Check {
        /// Print the effective configuration after file, environment, and
        /// flag layering — the answer to "which value actually won?".
        #[arg(long)]
        print_config: bool,
    },
    /// Run the Lua tests against an in-process server.
    Test,
    /// Apply pending SQL migrations from migrations/.
    Migrate {
        /// Report what has run and what is pending, applying nothing.
        #[arg(long)]
        status: bool,
    },
    /// Scaffold a new Nitr application.
    Init {
        /// Directory to scaffold into (default: the current directory).
        dir: Option<PathBuf>,
    },
    /// Package the application and this binary into one runnable file.
    Build {
        /// Path of the artifact to write.
        #[arg(short, long, value_name = "PATH")]
        output: PathBuf,
    },
    /// Ask a running server (found via its `pidfile`) to reload.
    Reload,
}

fn load_config(cli: &Cli) -> anyhow::Result<Config> {
    // A bundled executable carries its own application; the config file
    // and every path in it come from the extracted archive.
    let mut cfg = match bundle::load()? {
        Some(cfg) => cfg,
        None => match &cli.config {
            Some(path) => Config::from_file(path)?,
            None => {
                let default = Path::new(DEFAULT_CONFIG_FILE);
                if default.is_file() {
                    Config::from_file(default)?
                } else {
                    Config::default()
                }
            }
        },
    };
    cfg.apply_env()?;
    if cli.dev || matches!(cli.command, Some(Command::Dev)) {
        cfg.dev_mode = true;
    }
    Ok(cfg)
}

/// Installs the tracing subscriber per the `[log]` configuration.
/// `RUST_LOG` wins over the configured level; without either the default
/// is `info` (`debug` in dev mode).
fn init_logging(cfg: Option<&Config>, dev: bool) {
    let fallback = || {
        let configured = cfg.and_then(|c| c.log.level.clone());
        tracing_subscriber::EnvFilter::new(configured.unwrap_or_else(|| {
            if dev || cfg.is_some_and(|c| c.dev_mode) {
                "debug".into()
            } else {
                "info".into()
            }
        }))
    };
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| fallback());
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match cfg.map(|c| c.log.format) {
        Some(nitr::LogFormat::Json) => builder.json().init(),
        _ => builder.init(),
    }
}

/// Writes the pidfile on creation, removes it on drop — including the
/// error path, so a crashed server does not leave a stale pid behind for
/// `nitr reload` to signal.
struct Pidfile(PathBuf);

impl Pidfile {
    fn write(path: &Path) -> anyhow::Result<Self> {
        std::fs::write(path, format!("{}\n", std::process::id()))
            .with_context(|| format!("cannot write the pidfile {}", path.display()))?;
        Ok(Self(path.to_path_buf()))
    }
}

impl Drop for Pidfile {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

/// Sends SIGHUP to the process named by the configured `pidfile`.
fn reload(cfg: &Config) -> anyhow::Result<()> {
    let path = cfg.pidfile.as_ref().context(
        "no `pidfile` is configured: set `pidfile` in nitr.toml so `nitr reload` \
         can find the running server",
    )?;
    let raw = std::fs::read_to_string(path).with_context(|| {
        format!(
            "cannot read the pidfile {} (is the server running?)",
            path.display()
        )
    })?;
    let pid: u32 = raw
        .trim()
        .parse()
        .with_context(|| format!("the pidfile {} does not contain a pid", path.display()))?;

    #[cfg(unix)]
    {
        let status = std::process::Command::new("kill")
            .args(["-HUP", &pid.to_string()])
            .status()
            .context("cannot run `kill`")?;
        if !status.success() {
            bail!("kill -HUP {pid} failed: is the server still running?");
        }
        println!("sent SIGHUP to pid {pid}: the server is rebuilding its runtime pool");
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        bail!("`nitr reload` needs Unix signals, which this platform does not have");
    }
}

#[tokio::main]
async fn main() {
    if let Err(err) = run_main().await {
        // Print the report ourselves instead of returning the error:
        // diagnostics get color on a terminal (plain text otherwise, same
        // bytes anyhow would have printed).
        diag::report(&err);
        std::process::exit(1);
    }
}

async fn run_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if cli.version {
        println!("nitr {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // `init` runs before any configuration exists; everything else loads
    // the configuration first so `[log]` can shape the subscriber.
    if let Some(Command::Init { dir }) = &cli.command {
        init_logging(None, cli.dev);
        return init(dir.as_deref().unwrap_or(Path::new(".")));
    }

    let cfg = match load_config(&cli) {
        Ok(cfg) => {
            init_logging(Some(&cfg), cli.dev);
            cfg
        }
        Err(err) => {
            init_logging(None, cli.dev);
            return Err(err);
        }
    };

    match cli.command.unwrap_or(Command::Run) {
        Command::Init { .. } => unreachable!("handled above"),
        Command::Run | Command::Dev => {
            let pidfile_path = cfg.pidfile.clone();
            let server = Server::builder().config(cfg).build().await?;
            // Written only once the build succeeded: a pid that never
            // served is not one `nitr reload` should be signalling.
            let pidfile = pidfile_path.as_deref().map(Pidfile::write).transpose()?;
            let result = server.serve().await;
            drop(pidfile);
            result?;
        }
        Command::Check { print_config } => {
            if print_config {
                print!("{}", cfg.effective_toml()?);
                return Ok(());
            }
            check(cfg).await?;
        }
        Command::Test => {
            let failures = run_tests(cfg).await?;
            if failures > 0 {
                std::process::exit(1);
            }
        }
        Command::Migrate { status } => migrate(&cfg, status)?,
        Command::Build { output } => {
            let cfg_path = cli
                .config
                .as_deref()
                .unwrap_or(Path::new(DEFAULT_CONFIG_FILE));
            if !cfg_path.is_file() {
                bail!(
                    "`nitr build` needs a configuration file ({} not found): the \
                     bundle records it as the application manifest",
                    cfg_path.display()
                );
            }
            bundle::build(cfg_path, &cfg, &output)?;
        }
        Command::Reload => reload(&cfg)?,
    }
    Ok(())
}

/// Applies pending migrations, or reports their state with `--status`.
///
/// Deliberately separate from `nitr run`: applying schema changes at boot
/// means a rolling deployment has two instances racing to change the same
/// schema, each believing it is alone.
#[cfg(not(feature = "db"))]
fn migrate(_cfg: &Config, _status_only: bool) -> anyhow::Result<()> {
    bail!(
        "this build has no database support: rebuild with the `db` Cargo \
         feature (or `all`) to use `nitr migrate`"
    )
}

#[cfg(feature = "db")]
fn migrate(cfg: &Config, status_only: bool) -> anyhow::Result<()> {
    let db = cfg
        .database
        .as_ref()
        .context("no database is configured; set `database` in nitr.toml")?;
    let dir = db.migrations().context(
        "no migrations directory found (looked for `migrations/`; set \
         [database] migrations_dir to point elsewhere)",
    )?;
    let conn = nitr::stdlib::db_open(&db.path, &db.pragmas())?;

    if status_only {
        let entries = nitr::stdlib::migrate::status(&conn, &dir)?;
        if entries.is_empty() {
            println!("no migrations in {}", dir.display());
            return Ok(());
        }
        for (migration, state) in &entries {
            let label = match state {
                nitr::stdlib::migrate::State::Applied => "applied",
                nitr::stdlib::migrate::State::Pending => "pending",
                nitr::stdlib::migrate::State::Modified => "MODIFIED SINCE APPLIED",
            };
            println!("  {:<10} {}", label, migration.name);
        }
        let count = |wanted: nitr::stdlib::migrate::State| {
            entries.iter().filter(|(_, state)| *state == wanted).count()
        };
        let modified = count(nitr::stdlib::migrate::State::Modified);
        println!(
            "{} applied, {} pending, {modified} modified",
            count(nitr::stdlib::migrate::State::Applied),
            count(nitr::stdlib::migrate::State::Pending),
        );
        if modified > 0 {
            // Not a warning to skim past: the database and the repository
            // disagree about what the schema is.
            println!(
                "a modified migration will not be re-run; restore the file or write a new one"
            );
        }
        return Ok(());
    }

    let applied = nitr::stdlib::migrate::run(&conn, &dir)?;
    if applied.is_empty() {
        println!("ok: the schema is up to date");
    } else {
        println!("ok: applied {} migration(s)", applied.len());
        for name in applied {
            println!("  {name}");
        }
    }
    Ok(())
}

/// Validates the whole application by performing a real build: config
/// parsing, builtins resolution, Lua syntax, route conflicts, template and
/// database wiring. Note: the configuration script runs once (its side
/// effects, e.g. migrations, happen).
async fn check(cfg: Config) -> anyhow::Result<()> {
    let workers = cfg.workers;
    let cfg = Config { workers: 1, ..cfg };
    Server::builder()
        .config(cfg)
        .build()
        .await
        .context("check failed")?;
    println!("ok: configuration and scripts load cleanly ({workers} worker(s) configured)");
    Ok(())
}

/// Runs `*.lua` files from the tests directory against an in-process
/// server: each file gets a fresh sandboxed state with the configured
/// builtins plus a `test` global whose `test.request(method, path, opts?)`
/// dispatches through the real router/middleware/handler path.
async fn run_tests(cfg: Config) -> anyhow::Result<usize> {
    let tests_dir = cfg
        .tests_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("tests"));
    let mut files: Vec<PathBuf> = std::fs::read_dir(&tests_dir)
        .with_context(|| format!("cannot read the tests directory {}", tests_dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "lua"))
        .collect();
    files.sort();
    if files.is_empty() {
        bail!("no *.lua test files in {}", tests_dir.display());
    }

    let builtins = cfg.builtins()?;
    let env = BuiltinsEnv {
        templates_dir: cfg.templates_dir.clone(),
        database: cfg.database.as_ref().map(|db| db.path.clone()),
        sqlite: cfg
            .database
            .as_ref()
            .map(|db| db.pragmas())
            .unwrap_or_default(),
        fetch: cfg.fetch.options(),
        // Tests get their own cache: a test file must not see entries a
        // previous one left behind.
        cache: Some(nitr::stdlib::Cache::new(cfg.cache_options())),
    };
    let opts = cfg.runtime_opts()?;

    let server = Server::builder().config(cfg).build().await?;
    let client = server.test_client();

    let mut failures = 0usize;
    for file in &files {
        // A fresh state per file: tests are isolated from each other but
        // share the server (and its database) like real requests do.
        let mut rt = Runtime::new_with(runtime_opts_like(&opts)?)?;
        nitr::stdlib::register_builtins(rt.lua(), builtins, &env)?;
        register_test_global(rt.lua(), client.clone())?;

        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| file.display().to_string());
        let source = std::fs::read(file)
            .with_context(|| format!("cannot read test file {}", file.display()))?;
        let chunk = match rt.lua().load(source).into_function() {
            Ok(chunk) => chunk,
            Err(err) => {
                failures += 1;
                println!("FAIL {name}\n     {err}");
                continue;
            }
        };
        match rt.call_function::<mlua::Value>(chunk, ()).await {
            Ok(_) => println!("PASS {name}"),
            Err(err) => {
                failures += 1;
                println!("FAIL {name}\n     {err}");
            }
        }
    }
    println!(
        "\n{} passed, {} failed ({} file(s))",
        files.len() - failures,
        failures,
        files.len()
    );
    Ok(failures)
}

/// `RuntimeOpts` is not `Clone`; rebuild an equivalent one.
fn runtime_opts_like(opts: &nitr::RuntimeOpts) -> anyhow::Result<nitr::RuntimeOpts> {
    Ok(nitr::RuntimeOpts {
        libs: opts.libs,
        memory_limit: opts.memory_limit,
        dev_mode: opts.dev_mode,
        exec_timeout: opts.exec_timeout,
        package_dir: opts.package_dir.clone(),
    })
}

/// Mounts `nitr.test` for test scripts: `nitr.test.request(method, path,
/// opts?)` with opts `{ headers = {...}, body = "..." }` returning
/// `{ status, headers, body }`.
fn register_test_global(lua: &mlua::Lua, client: nitr::testing::TestClient) -> anyhow::Result<()> {
    let test = lua.create_table()?;
    test.set(
        "request",
        lua.create_async_function(
            move |lua, (method, path, opts): (String, String, Option<mlua::Table>)| {
                let client = client.clone();
                async move {
                    use mlua::ExternalResult as _;
                    let mut headers = Vec::new();
                    let mut body = None;
                    if let Some(opts) = opts {
                        if let Some(header_table) = opts.get::<Option<mlua::Table>>("headers")? {
                            for pair in header_table.pairs::<String, String>() {
                                let (k, v) = pair?;
                                headers.push((k, v));
                            }
                        }
                        if let Some(raw) = opts.get::<Option<mlua::LuaString>>("body")? {
                            body = Some(bytes::Bytes::copy_from_slice(&raw.as_bytes()));
                        }
                    }
                    let resp = client
                        .request(&method, &path, &headers, body)
                        .await
                        .into_lua_err()?;

                    let table = lua.create_table()?;
                    table.set("status", resp.status)?;
                    let header_table = lua.create_table()?;
                    for (k, v) in &resp.headers {
                        header_table.set(k.as_str(), v.as_str())?;
                    }
                    table.set("headers", header_table)?;
                    table.set("body", lua.create_string(&resp.body)?)?;
                    Ok(table)
                }
            },
        )?,
    )?;
    nitr::nitr_table(lua)?.set("test", test)?;
    Ok(())
}

/// Scaffolds a minimal Nitr application; refuses to overwrite anything.
fn init(dir: &Path) -> anyhow::Result<()> {
    let files: &[(&str, &str)] = &[
        ("nitr.toml", INIT_NITR_TOML),
        ("app.lua", INIT_APP_LUA),
        ("public/index.html", INIT_INDEX_HTML),
        ("tests/app_test.lua", INIT_TEST_LUA),
    ];
    for (rel, _) in files {
        if dir.join(rel).exists() {
            bail!("refusing to overwrite existing {}", dir.join(rel).display());
        }
    }
    for (rel, content) in files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
        println!("created {}", path.display());
    }
    println!("\nNext steps:\n  nitr check\n  nitr test\n  nitr dev");
    Ok(())
}

const INIT_NITR_TOML: &str = r#"# Nitr application configuration.
listen = "127.0.0.1:3000"
handler_script = "app.lua"

# Static files are served from this directory, before Lua runs.
[static]
dir = "public"
mount = "/"
"#;

const INIT_APP_LUA: &str = r#"local app = nitr.app()

app:use(function(next)
    return function(req)
        nitr.log.info("request", { path = req.path })
        return next(req)
    end
end)

app:get("/api/hello", function(req)
    return nitr.json({ hello = req.query.name or "world" })
end)

app:on_error(function(err, req)
    -- err is structured: err.kind ("lua"|"nitr"|"module"|"timeout"|"memory"|"panic"),
    -- err.message, err.source, err.line, err.module, err.traceback, err.cause.
    nitr.log.error("handler failed", {
        error = err.message, kind = err.kind, source = err.source, line = err.line,
    })
    return nitr.error(500, { code = "INTERNAL" })
end)

return app
"#;

const INIT_INDEX_HTML: &str = r#"<!doctype html>
<h1>Hello from Nitr</h1>
<p>Edit <code>public/index.html</code> and <code>app.lua</code>.</p>
"#;

const INIT_TEST_LUA: &str = r#"-- Run with: nitr test
local resp = nitr.test.request("GET", "/api/hello?name=nitr")
assert(resp.status == 200, "expected 200, got " .. resp.status)
assert(nitr.json:decode(resp.body).hello == "nitr", "unexpected body: " .. resp.body)

local resp = nitr.test.request("GET", "/")
assert(resp.status == 200, "static index should be served")
"#;
