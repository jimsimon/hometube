//! Domain models.
//!
//! Each submodule wraps a database table with strongly-typed Rust structs
//! and helpers. Phase 2 introduces [`account`] and [`session`]; later
//! phases populate the rest.

pub mod account;
pub mod session;
pub mod video;
