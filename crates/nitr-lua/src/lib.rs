//! The built-in `nitr.*` standard library for Nitr and its registration.
//!
//! Every builtin mounts as a field of the global `nitr` namespace table
//! (`nitr.json`, `nitr.fetch`, `nitr.db`, …) — Nitr registers no other
//! globals, so scripts always read `nitr.*` and nothing is intermixed with
//! the Lua standard library.

#![forbid(unsafe_code)]
#![deny(warnings)]
#![deny(rust_2018_idioms)]
#![deny(dead_code)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

use std::path::PathBuf;

use nitr_core::Result;

pub(crate) mod crypto;
pub(crate) mod db;
pub(crate) mod fetch;
pub(crate) mod http;
pub(crate) mod json;
pub(crate) mod log;
pub(crate) mod template;
pub(crate) mod utils;

pub use fetch::FetchOptions;
pub use http::{best_match, RequestCookies, ResponseCookies};

bitflags::bitflags! {
    /// Built-in `nitr.*` standard library modules that can be exposed to
    /// Lua scripts.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Builtins: u32 {
        /// `nitr.dbg(value)` debug-print function.
        const DEBUG = 1;
        /// `nitr.fetch(method, url, opts?)` HTTP client plus
        /// `nitr.await_all` for concurrent requests.
        const FETCH = 1 << 1;
        /// `nitr.template:render(name, data?)` template engine (minijinja).
        const TEMPLATE = 1 << 2;
        /// `nitr.json` JSON codec (`:encode`/`:decode`) and, called as a
        /// function, the JSON response helper.
        const JSON = 1 << 3;
        /// `nitr.db:execute/query/query_row/query_one/transaction` SQLite
        /// driver.
        const DATABASE = 1 << 4;
        /// HTTP ergonomics: the `nitr.text`/`html`/`redirect`/`status`/
        /// `negotiate`/`sse` response helpers and `nitr.error`.
        const HTTP = 1 << 5;
        /// `nitr.log.debug/info/warn/error(msg, fields?)` structured logging.
        const LOG = 1 << 6;
        /// `nitr.crypto` primitives (hashing, HMAC, passwords) and the
        /// `nitr.auth` header parsers.
        const CRYPTO = 1 << 7;
    }
}

impl Builtins {
    /// The field name a builtin mounts under on the `nitr` namespace table.
    ///
    /// Returns `None` for combined flags and for builtins that register
    /// several fields ([`HTTP`](Self::HTTP) and [`CRYPTO`](Self::CRYPTO)).
    pub fn nitr_name(self) -> Option<&'static str> {
        match self {
            Builtins::DEBUG => Some("dbg"),
            Builtins::FETCH => Some("fetch"),
            Builtins::TEMPLATE => Some("template"),
            Builtins::JSON => Some("json"),
            Builtins::DATABASE => Some("db"),
            Builtins::LOG => Some("log"),
            _ => None,
        }
    }

    /// Resolves a configuration name (e.g. `"dbg"`, `"fetch"`, `"db"`) into
    /// its builtin flag.
    pub fn from_config_name(name: &str) -> Option<Self> {
        match name {
            "dbg" => Some(Builtins::DEBUG),
            "fetch" => Some(Builtins::FETCH),
            "template" => Some(Builtins::TEMPLATE),
            "json" => Some(Builtins::JSON),
            "db" => Some(Builtins::DATABASE),
            "http" => Some(Builtins::HTTP),
            "log" => Some(Builtins::LOG),
            "crypto" => Some(Builtins::CRYPTO),
            _ => None,
        }
    }
}

/// External resources required by some builtins: `template` needs a
/// templates directory and `db` a SQLite database file.
#[derive(Debug, Clone, Default)]
pub struct BuiltinsEnv {
    /// Directory the `template` builtin loads templates from.
    pub templates_dir: Option<PathBuf>,
    /// SQLite database file the `conn` builtin connects to.
    pub database: Option<PathBuf>,
    /// Outbound-request policy for the `fetch` builtin.
    pub fetch: FetchOptions,
}

/// Registers the selected builtins as fields of the global `nitr`
/// namespace table (`nitr.dbg`, `nitr.fetch`, `nitr.json`, `nitr.db`, …).
///
/// Builtins that need a setting from the [`BuiltinsEnv`] (`template` needs
/// `templates_dir`, `db` needs `database`) are skipped with a warning when
/// that setting is absent; callers that take an explicit builtins list
/// should reject such combinations upfront.
pub fn register_builtins(lua: &mlua::Lua, builtins: Builtins, env: &BuiltinsEnv) -> Result {
    let nitr = nitr_core::nitr_table(lua)?;
    for builtin in builtins.iter() {
        match builtin {
            Builtins::DEBUG => nitr.set("dbg", utils::create_debug_fn(lua)?)?,
            // Also registers `nitr.await_all` for concurrent requests.
            Builtins::FETCH => {
                let opts = std::sync::Arc::new(env.fetch.clone());
                nitr.set("fetch", fetch::create_fetch_fn(lua, opts.clone())?)?;
                nitr.set("await_all", fetch::create_await_all_fn(lua, opts)?)?;
            }
            Builtins::TEMPLATE => match &env.templates_dir {
                Some(dir) => nitr.set("template", template::create_template_fn(lua, dir)?)?,
                None => {
                    tracing::warn!(
                        "skipping builtin `template`: `templates_dir` is not configured"
                    );
                }
            },
            Builtins::JSON => nitr.set("json", json::create_json_fn(lua)?)?,
            // Registers the response helpers (`nitr.text`, `nitr.html`,
            // `nitr.redirect`, `nitr.status`, `nitr.negotiate`, `nitr.sse`)
            // and `nitr.error`.
            Builtins::HTTP => http::register(lua, &nitr)?,
            Builtins::LOG => nitr.set("log", log::create_log_table(lua)?)?,
            // Registers both `nitr.crypto` and `nitr.auth`.
            Builtins::CRYPTO => {
                nitr.set("crypto", crypto::create_crypto_table(lua)?)?;
                nitr.set("auth", crypto::create_auth_table(lua)?)?;
            }
            Builtins::DATABASE => match &env.database {
                Some(path) => nitr.set("db", db::create_database_fn(lua, path)?)?,
                None => {
                    tracing::warn!("skipping builtin `db`: `database` is not configured");
                }
            },
            _ => continue,
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Builtins;

    #[test]
    fn config_names_round_trip() {
        for (name, flag) in [
            ("dbg", Builtins::DEBUG),
            ("fetch", Builtins::FETCH),
            ("template", Builtins::TEMPLATE),
            ("json", Builtins::JSON),
            ("db", Builtins::DATABASE),
            ("http", Builtins::HTTP),
            ("log", Builtins::LOG),
            ("crypto", Builtins::CRYPTO),
        ] {
            assert_eq!(Builtins::from_config_name(name), Some(flag));
        }
        assert_eq!(Builtins::from_config_name("nope"), None);
        // Combined flags and multi-field builtins have no single name.
        assert_eq!((Builtins::DEBUG | Builtins::JSON).nitr_name(), None);
        assert_eq!(Builtins::HTTP.nitr_name(), None);
        assert_eq!(Builtins::DATABASE.nitr_name(), Some("db"));
    }
}
