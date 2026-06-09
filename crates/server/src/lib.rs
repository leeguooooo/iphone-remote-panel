pub mod config;
pub mod http;
pub mod input_bridge;
pub mod macos;
pub mod protocol;
pub mod runtime_dir;
pub mod signaling;
pub mod turn;
pub mod webrtc;

/// Re-export of the `core` crate under a non-`core` name.
///
/// Downstream integration-test crates that depend on `server` need the core
/// types but must NOT bring a dependency literally named `core` into their
/// extern prelude — doing so shadows the std `core` crate and breaks any
/// `core::`-emitting macro (e.g. `#[tokio::test]`). Reaching the core types
/// through this alias avoids that footgun.
pub use ::core as core_crate;
