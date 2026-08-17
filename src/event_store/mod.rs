//! Event-store abstractions and implementations.
//!
//! Expected shape: an `EventStore` trait with append/load/replay operations,
//! plus a v0 backend such as JSONL or SQLite. Event envelopes should carry
//! stream/aggregate id, sequence/version, timestamps, causation/correlation
//! ids, and schema version.
