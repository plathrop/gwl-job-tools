//! Core library for `gwl-jobs`.
//!
//! The binary in `main.rs` should stay thin: parse CLI, initialize telemetry,
//! dispatch to the command layer, and shut telemetry down.

pub mod cli;
pub mod config;
pub mod domain;
pub mod error;
pub mod event_store;
pub mod projections;
pub mod telemetry;
