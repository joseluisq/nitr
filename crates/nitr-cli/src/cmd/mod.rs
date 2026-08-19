//! One module per `nitr` subcommand implementation; `main.rs` keeps only
//! argument parsing, configuration loading, and dispatch.

pub(crate) mod check;
pub(crate) mod migrate;
pub(crate) mod test;
