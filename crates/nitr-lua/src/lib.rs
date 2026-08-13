//! Built-in Lua globals (userdata/functions) for Nitr and their registration.

#![forbid(unsafe_code)]
#![deny(warnings)]
#![deny(rust_2018_idioms)]
#![deny(dead_code)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

use std::path::PathBuf;

use nitr_core::Result;

pub(crate) mod db;
pub(crate) mod fetch;
pub(crate) mod http;
pub(crate) mod json;
pub(crate) mod template;
pub(crate) mod utils;

pub use http::{best_match, RequestCookies, ResponseCookies};

bitflags::bitflags! {
    /// Built-in globals that can be exposed to Lua scripts.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Builtins: u32 {
        /// `dbg(value)` debug-print function.
        const DEBUG = 1;
        /// `fetch(method, url, headers?)` HTTP client.
        const FETCH = 1 << 1;
        /// `template:render(name, data?)` template engine (minijinja).
        const TEMPLATE = 1 << 2;
        /// `json:encode(value)` / `json:decode(string)` JSON codec.
        const JSON = 1 << 3;
        /// `conn:execute/query/query_row/query_one` SQLite driver.
        const DATABASE = 1 << 4;
        /// HTTP ergonomics: the `text`/`html`/`redirect`/`status`/`negotiate`
        /// response helpers and the `http` table (`http.error`).
        const HTTP = 1 << 5;
    }
}

impl Builtins {
    /// The Lua global variable name a builtin is registered under.
    ///
    /// Returns `None` unless `self` is a single flag.
    pub fn global_name(self) -> Option<&'static str> {
        match self {
            Builtins::DEBUG => Some("dbg"),
            Builtins::FETCH => Some("fetch"),
            Builtins::TEMPLATE => Some("template"),
            Builtins::JSON => Some("json"),
            Builtins::DATABASE => Some("conn"),
            Builtins::HTTP => Some("http"),
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
}

/// Registers the selected **built-in** globals (`dbg`, `fetch`, `template`,
/// `json`, `conn`) into a Lua state.
///
/// Builtins that need a setting from the [`BuiltinsEnv`] (`template` needs
/// `templates_dir`, `db` needs `database`) are skipped with a warning when
/// that setting is absent; callers that take an explicit builtins list
/// should reject such combinations upfront.
pub fn register_builtins(lua: &mlua::Lua, builtins: Builtins, env: &BuiltinsEnv) -> Result {
    let globals = lua.globals();
    for builtin in builtins.iter() {
        let Some(name) = builtin.global_name() else {
            continue;
        };
        match builtin {
            Builtins::DEBUG => globals.set(name, utils::create_debug_fn(lua)?)?,
            Builtins::FETCH => globals.set(name, fetch::create_fetch_fn(lua)?)?,
            Builtins::TEMPLATE => match &env.templates_dir {
                Some(dir) => globals.set(name, template::create_template_fn(lua, dir)?)?,
                None => {
                    tracing::warn!(
                        "skipping builtin `template`: `templates_dir` is not configured"
                    );
                }
            },
            Builtins::JSON => globals.set(name, json::create_json_fn(lua)?)?,
            // Registers several globals (`text`, `html`, `redirect`,
            // `status`, `negotiate`, `http`), not just `http`.
            Builtins::HTTP => http::register(lua)?,
            Builtins::DATABASE => match &env.database {
                Some(path) => globals.set(name, db::create_database_fn(lua, path)?)?,
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
        ] {
            assert_eq!(Builtins::from_config_name(name), Some(flag));
            assert!(flag.global_name().is_some());
        }
        assert_eq!(Builtins::from_config_name("nope"), None);
        // Combined flags have no single global name.
        assert_eq!((Builtins::DEBUG | Builtins::JSON).global_name(), None);
        assert_eq!(Builtins::DATABASE.global_name(), Some("conn"));
    }
}
