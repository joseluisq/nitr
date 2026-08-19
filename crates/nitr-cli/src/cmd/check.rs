//! `nitr check`: prove the configuration and scripts load cleanly.

use anyhow::Context as _;
use nitr::{Config, Server};

/// Validates the whole application by performing a real build: config
/// parsing, builtins resolution, Lua syntax, route conflicts, template and
/// database wiring. Note: the configuration script runs once (its side
/// effects, e.g. migrations, happen).
pub(crate) async fn check(cfg: Config) -> anyhow::Result<()> {
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
