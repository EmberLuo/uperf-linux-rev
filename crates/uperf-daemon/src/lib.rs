//! Daemon orchestration, reducer, policy reconciliation, and D-Bus service.

pub mod auth;
pub mod config;
pub mod config_watch;
pub mod conflicts;
mod decision_trace;
pub mod logging;
pub mod observers;
mod reconcile;
pub mod runtime;
pub mod service;
