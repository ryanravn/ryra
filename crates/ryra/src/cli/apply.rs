//! Step execution lives in `ryra_core::system::apply` so every frontend
//! (CLI, HTTP API) shares one executor. This shim keeps the
//! long-standing `cli::apply::*` call sites working.

pub use ryra_core::system::apply::*;
