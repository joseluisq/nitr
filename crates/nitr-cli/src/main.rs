use anyhow::{bail, Context as _};
use std::path::{Path, PathBuf};

use nitr::{Config, Server};

const DEFAULT_CONFIG_FILE: &str = "nitr.toml";

const USAGE: &str = "\
Usage: nitr [OPTIONS]

Options:
  -c, --config <PATH>  Path to the TOML config file (default: ./nitr.toml)
      --dev            Enable development mode (hot reload)
  -h, --help           Print this help message";

struct Args {
    config: Option<PathBuf>,
    dev: bool,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args {
        config: None,
        dev: false,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = load_config(&parse_args()?)?;
    let server = Server::builder().config(cfg).build().await?;
    server.serve().await?;
    Ok(())
}
