use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};

use nitr::{BuiltinsEnv, Config, Runtime, Server};

const DEFAULT_CONFIG_FILE: &str = "nitr.toml";

const USAGE: &str = "\
Usage: nitr [COMMAND] [OPTIONS]

Commands:
  run              Start the server (default)
  dev              Start the server in development mode (hot reload)
  check            Load the configuration and scripts, then exit
  test             Run the Lua tests against an in-process server
  init [DIR]       Scaffold a new Nitr application

Options:
  -c, --config <PATH>  Path to the TOML config file (default: ./nitr.toml)
      --dev            Enable development mode (hot reload)
  -h, --help           Print this help message

Signals:
  SIGHUP           Zero-downtime reload: rebuilds the Lua runtime pool";

struct Args {
    command: Command,
    config: Option<PathBuf>,
    dev: bool,
}

enum Command {
    Run,
    Check,
    Test,
    Init(PathBuf),
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args {
        command: Command::Run,
        config: None,
        dev: false,
    };
    let mut iter = std::env::args().skip(1).peekable();
    let mut command_seen = false;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "run" if !command_seen => command_seen = true,
            "dev" if !command_seen => {
                command_seen = true;
                args.dev = true;
            }
            "check" if !command_seen => {
                command_seen = true;
                args.command = Command::Check;
            }
            "test" if !command_seen => {
                command_seen = true;
                args.command = Command::Test;
            }
            "init" if !command_seen => {
                command_seen = true;
                let dir = match iter.peek() {
                    Some(next) if !next.starts_with('-') => {
                        PathBuf::from(iter.next().expect("peeked"))
                    }
                    _ => PathBuf::from("."),
                };
                args.command = Command::Init(dir);
            }
            "-c" | "--config" => {
                let path = iter
                    .next()
                    .with_context(|| format!("missing value for {arg}"))?;
                args.config = Some(PathBuf::from(path));
            }
            "--dev" => args.dev = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ => bail!("unknown argument `{arg}`\n{USAGE}"),
        }
    }
    Ok(args)
}

fn load_config(args: &Args) -> anyhow::Result<Config> {
    let mut cfg = match &args.config {
        Some(path) => Config::from_file(path)?,
        None => {
            let default = Path::new(DEFAULT_CONFIG_FILE);
            if default.is_file() {
                Config::from_file(default)?
            } else {
                Config::default()
            }
        }
    };
    cfg.apply_env()?;
    if args.dev {
        cfg.dev_mode = true;
    }
    Ok(cfg)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = parse_args()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(if args.dev { "debug" } else { "info" })
            }),
        )
        .init();

    match &args.command {
        Command::Init(dir) => return init(dir),
        Command::Run => {
            let cfg = load_config(&args)?;
            let server = Server::builder().config(cfg).build().await?;
            server.serve().await?;
        }
        Command::Check => {
            let cfg = load_config(&args)?;
            check(cfg).await?;
        }
        Command::Test => {
            let cfg = load_config(&args)?;
            let failures = run_tests(cfg).await?;
            if failures > 0 {
                std::process::exit(1);
            }
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
        database: cfg.database.clone(),
        fetch: cfg.fetch.options(),
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
    nitr.log.error("handler failed", { error = err })
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
