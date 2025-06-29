// #![deny(missing_docs)]
#![forbid(unsafe_code)]
#![deny(warnings)]
#![deny(rust_2018_idioms)]
#![deny(dead_code)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

pub(crate) mod error;
pub(crate) mod handler;
pub(crate) mod runtime;

// Extern crates
pub mod service;
pub mod userdata;

// Re-exports
pub use error::*;
pub use runtime::Runtime;
