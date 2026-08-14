//! The Nitr HTTP server layer: hyper server and builder, configuration,
//! and the request/response bridge between HTTP and the Lua runtime.

#![forbid(unsafe_code)]
#![deny(warnings)]
#![deny(rust_2018_idioms)]
#![deny(dead_code)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

pub(crate) mod app;
pub(crate) mod config;
pub(crate) mod handler;
pub(crate) mod protect;
pub(crate) mod request;
pub(crate) mod server;
pub(crate) mod stream;

pub mod service;

pub use config::{Config, FetchConfig, LimitsConfig, LuaConfig, RateLimitConfig};
pub use server::{Server, ServerBuilder};
