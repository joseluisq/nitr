use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use clap::{Parser, Subcommand};

use nitr::{BuiltinsEnv, Config, Runtime, Server};

mod apidef;
mod bundle;
mod diag;
mod scaffold;

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
    Test {
        /// Run only tests whose name (or file name) contains this string.
        #[arg(long, value_name = "SUBSTRING")]
        filter: Option<String>,
    },
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
        /// Only the bare minimum: nitr.toml, app.lua, a static page, one
        /// test — instead of the full documented layout.
        #[arg(long)]
        minimal: bool,
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
    if let Some(Command::Init { dir, minimal }) = &cli.command {
        init_logging(None, cli.dev);
        return scaffold::init(dir.as_deref().unwrap_or(Path::new(".")), *minimal);
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
        Command::Test { filter } => {
            let failures = run_tests(cfg, filter.as_deref()).await?;
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
/// The Lua test framework (`t.describe`/`t.it`/`t.expect`/hooks), loaded
/// into each test state before its file runs.
const TEST_FRAMEWORK: &str = include_str!("test_framework.lua");

/// One `t.it(...)` outcome, read back from the state after the file ran.
struct TestOutcome {
    name: String,
    ok: bool,
    skipped: bool,
    err: Option<String>,
}

/// Reads `nitr.test._results` from a finished test state.
fn collect_outcomes(lua: &mlua::Lua) -> anyhow::Result<Vec<TestOutcome>> {
    let results: mlua::Table = nitr::nitr_table(lua)?
        .get::<mlua::Table>("test")?
        .get("_results")?;
    let mut out = Vec::new();
    for entry in results.sequence_values::<mlua::Table>() {
        let entry = entry?;
        out.push(TestOutcome {
            name: entry.get("name")?,
            ok: entry.get::<Option<bool>>("ok")?.unwrap_or(false),
            skipped: entry.get::<Option<bool>>("skipped")?.unwrap_or(false),
            err: entry.get("err")?,
        });
    }
    Ok(out)
}

async fn run_tests(cfg: Config, filter: Option<&str>) -> anyhow::Result<usize> {
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

    let (mut passed, mut failed, mut skipped) = (0usize, 0usize, 0usize);
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

        // The framework rides on `nitr.test`, with the runner's filter and
        // the file name injected before it loads.
        let test_table: mlua::Table = nitr::nitr_table(rt.lua())?.get("test")?;
        test_table.set("_filter", filter.unwrap_or_default())?;
        test_table.set("_file", name.as_str())?;
        rt.lua()
            .load(TEST_FRAMEWORK)
            .set_name("@nitr-test-framework")
            .exec()
            .context("cannot load the test framework")?;

        let source = std::fs::read(file)
            .with_context(|| format!("cannot read test file {}", file.display()))?;
        // Named after the real file, so assertion failures point at it.
        let chunk = match rt
            .lua()
            .load(source)
            .set_name(format!("@{}", file.display()))
            .into_function()
        {
            Ok(chunk) => chunk,
            Err(err) => {
                failed += 1;
                println!("FAIL {name}\n     {err}");
                continue;
            }
        };
        let file_err = rt.call_function::<mlua::Value>(chunk, ()).await.err();

        let outcomes = collect_outcomes(rt.lua())?;
        if outcomes.is_empty() {
            // The pre-framework style: a bare script of asserts. It passes
            // by running to completion.
            match file_err {
                None => {
                    passed += 1;
                    println!("PASS {name}");
                }
                Some(err) => {
                    failed += 1;
                    println!("FAIL {name}\n     {err}");
                }
            }
            continue;
        }
        println!("{name}");
        for outcome in outcomes {
            if outcome.skipped {
                skipped += 1;
                continue;
            }
            if outcome.ok {
                passed += 1;
                println!("  ok   {}", outcome.name);
            } else {
                failed += 1;
                println!("  FAIL {}", outcome.name);
                if let Some(err) = outcome.err {
                    for line in err.lines() {
                        println!("       {line}");
                    }
                }
            }
        }
        // A file that also failed outside any `it` (e.g. in `describe`
        // setup) is its own failure, on top of whatever tests recorded.
        if let Some(err) = file_err {
            failed += 1;
            println!("  FAIL {name} (outside any test)\n     {err}");
        }
    }
    match skipped {
        0 => println!(
            "\n{passed} passed, {failed} failed ({} file(s))",
            files.len()
        ),
        _ => println!(
            "\n{passed} passed, {failed} failed, {skipped} filtered out ({} file(s))",
            files.len()
        ),
    }
    Ok(failed)
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
                        // `json = <value>` encodes the body and sets the
                        // content type, so tests stop hand-encoding both.
                        let json = opts.get::<mlua::Value>("json")?;
                        if !json.is_nil() {
                            let encoded = serde_json::to_vec(&json).into_lua_err()?;
                            body = Some(bytes::Bytes::from(encoded));
                            if !headers
                                .iter()
                                .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                            {
                                headers.push(("content-type".into(), "application/json".into()));
                            }
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
                    // resp:json() — the decoded body, so a test reads
                    // `resp:json().name` instead of decoding by hand.
                    table.set(
                        "json",
                        lua.create_function(|lua, this: mlua::Table| {
                            let body: mlua::LuaString = this.get("body")?;
                            let value =
                                serde_json::from_slice::<serde_json::Value>(&body.as_bytes())
                                    .into_lua_err()?;
                            use mlua::LuaSerdeExt as _;
                            lua.to_value(&value)
                        })?,
                    )?;
                    Ok(table)
                }
            },
        )?,
    )?;
    nitr::nitr_table(lua)?.set("test", test)?;
    Ok(())
}
