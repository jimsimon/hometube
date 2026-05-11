//! Tower middleware.
//!
//! - [`auth`]: session validation (resolves the signed cookie into a
//!   [`auth::CurrentAccount`] request extension)
//! - [`account_type`]: parent-vs-child role gates
//! - [`setup_redirect`]: forces every page through the setup wizard until
//!   the install is complete
//! - [`usage_limit`]: child-side daily-cap + allowed-window enforcement

pub mod account_type;
pub mod auth;
pub mod rate_limit;
pub mod setup_redirect;
pub mod usage_limit;
