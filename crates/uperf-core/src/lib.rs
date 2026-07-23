//! Pure domain types, configuration and policy for uperf-linux.
//!
//! This crate deliberately has no async runtime, D-Bus, Linux syscall, `/proc`, or
//! `/sys` dependency.  It describes what was observed, what policy wants, and what
//! was verified as applied; platform crates are responsible for obtaining and
//! mutating those states.

pub mod config;
pub mod domain;
pub mod migration;
pub mod policy;

pub use config::*;
pub use domain::*;
pub use migration::*;
pub use policy::*;
