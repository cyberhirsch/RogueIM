//! RogueIM core: identity, E2EE sessions, encrypted storage and the P2P engine.
//!
//! The UI talks to the engine through [`engine::spawn`], which returns a command
//! sender and an event receiver. The engine runs on its own thread with its own
//! tokio runtime, so any UI toolkit can drive it.

pub mod device;
pub mod engine;
pub mod identity;
pub mod mailbox;
pub mod proto;
pub mod store;

pub use engine::{spawn, Command, EngineConfig, EngineHandle, Event};
pub use proto::*;
