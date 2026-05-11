//! HomeTube library entry point.
//!
//! The application is split between a thin `main.rs` (process entry,
//! tracing init, scheduler wiring) and this library, which contains
//! every module integration tests need to reach. The split exists so
//! that integration tests in `tests/` can `use hometube::*;` and
//! exercise the same code the binary runs.

pub mod config;
pub mod db;
pub mod error;
pub mod middleware;
pub mod models;
pub mod routes;
pub mod services;
pub mod state;
