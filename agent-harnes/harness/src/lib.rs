//! Harness library: agent loop, memory, events, tools, and the proxy client.
//!
//! Phase 1 delivers the authenticated client.
//! Phase 2 adds the agent loop, memory trait, events, and transcript logging.

pub mod client;
pub mod memory;
pub mod events;
pub mod transcript;
pub mod agent_loop;
pub mod tools;
