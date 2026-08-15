// #![deny(missing_docs)]
#![forbid(unsafe_code)]
#![deny(warnings)]
#![deny(rust_2018_idioms)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

//! Nitr: a Rust web server embedding Lua for fast, efficient and safe
//! dynamic backends.
//!
//! This crate is the facade over the Nitr workspace and the usual single
//! dependency for applications and embedders:
//!
//! - [`nitr-core`](nitr_core) — the sandboxed Lua runtime and pool,
//! - [`nitr-std`](stdlib) — the built-in `nitr.*` standard library
//!   (`nitr.json`, `nitr.fetch`, `nitr.db`, …),
//! - [`nitr-http`](nitr_http) — the hyper server, configuration, and the
//!   HTTP/Lua bridge.

// Extern crates
pub use nitr_std as stdlib;

pub use nitr_http::service;
pub use nitr_http::testing;

// Re-exports
pub use nitr_core::{
    mount, nitr_table, DeadlineHandle, Error, ModuleFn, Result, Runtime, RuntimeGuard, RuntimeOpts,
    RuntimePool,
};
pub use nitr_http::{
    CacheConfig, CompressionConfig, Config, CorsConfig, DatabaseConfig, FetchConfig, LimitsConfig,
    LuaConfig, RateLimitConfig, Server, ServerBuilder, ShutdownConfig, StdConfig,
};
pub use nitr_std::{Builtins, BuiltinsEnv};
