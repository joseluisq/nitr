//! Built-in Lua globals (userdata/functions) and their registration.

pub(crate) mod db;
pub(crate) mod fetch;
pub(crate) mod json;
pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod template;
pub(crate) mod utils;

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
            _ => None,
        }
    }
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
