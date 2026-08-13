// #![deny(missing_docs)]
#![forbid(unsafe_code)]
#![deny(warnings)]
#![deny(rust_2018_idioms)]
#![deny(dead_code)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

pub(crate) mod config;
pub(crate) mod error;
pub(crate) mod handler;
pub(crate) mod runtime;
pub(crate) mod server;

// Extern crates
pub mod lua;
pub mod service;

// Re-exports
pub use config::{Config, LuaConfig};
pub use error::{Error, Result};
pub use lua::Builtins;
pub use runtime::{Runtime, RuntimeGuard, RuntimeOpts, RuntimePool};
pub use server::{Server, ServerBuilder};
