//! The resource-controlled Lua runtime at the heart of Nitr: sandboxed
//! states with memory/execution limits and the fixed runtime pool.
//!
//! This crate is deliberately free of HTTP, database, and template
//! dependencies so it can be embedded on its own; the `nitr` facade crate
//! is the usual entrypoint for applications.

#![forbid(unsafe_code)]
#![deny(warnings)]
#![deny(rust_2018_idioms)]
#![deny(dead_code)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod error;
mod runtime;

pub use error::{Error, Result};
pub use runtime::{DeadlineHandle, Runtime, RuntimeGuard, RuntimeOpts, RuntimePool};
