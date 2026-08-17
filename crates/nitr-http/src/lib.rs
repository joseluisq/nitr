//! The Nitr HTTP server layer: hyper server and builder, configuration,
//! and the request/response bridge between HTTP and the Lua runtime.

#![forbid(unsafe_code)]
#![deny(warnings)]
#![deny(rust_2018_idioms)]
#![deny(dead_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub(crate) mod app;
pub(crate) mod compress;
pub(crate) mod config;
pub(crate) mod cors;
pub(crate) mod handler;
pub(crate) mod health;
#[cfg(feature = "multipart")]
pub(crate) mod multipart;
pub(crate) mod protect;
pub(crate) mod range;
pub(crate) mod request;
pub(crate) mod server;
pub(crate) mod static_files;
pub(crate) mod stream;

pub mod testing;

pub mod service;

pub use config::{
    CacheConfig, CompressionConfig, Config, CorsConfig, DatabaseConfig, FetchConfig, HealthConfig,
    LimitsConfig, LogConfig, LogFormat, LuaConfig, RateLimitConfig, ShutdownConfig, StaticConfig,
    StdConfig,
};
pub use server::{Server, ServerBuilder};
