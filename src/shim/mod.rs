//! Shim runtime: owner, subscriber, and direct-passthrough modes plus
//! the supporting bits (frame protocol, console raw-mode guard, ConPTY
//! helper). The `claude.exe` bin (`src/bin/claude.rs`) chooses one of
//! the modes based on the planned argv and whether a per-session pipe
//! already exists for the target session id.

pub mod owner;
pub mod passthrough;
pub mod plan;
pub mod protocol;
pub mod subscriber;
pub mod util;
