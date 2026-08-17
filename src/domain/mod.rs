//! Domain types and aggregate behavior.
//!
//! This module will own commands, events, and pure aggregate functions such as
//! `decide` and `evolve`. Keep persistence and CLI concerns out of this module.

pub(crate) mod commands;
pub(crate) mod company;
pub(crate) mod compensation;
pub(crate) mod contact;
pub(crate) mod events;
pub(crate) mod role;
