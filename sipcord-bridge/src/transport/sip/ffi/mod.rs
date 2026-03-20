//! Low-level pjsua FFI wrapper
//!
//! This module provides safe(r) Rust wrappers around the pjsua C library.
//! Pure FFI code only — application-level logic lives in the parent `sip` module.
//!
//! ## Module Structure
//!
//! - `types` - Constants, statics, wrapper types, DigestAuthParams, CallbackHandlers
//! - `utils` - String conversion utilities
//! - `init` - PJSUA initialization, TLS transport, shutdown
//! - `direct_player` - Direct player port for join sounds
//! - `streaming_player` - Streaming player for large files
//! - `looping_player` - Looping player for early media
//! - `test_tone` - Test tone generator (440Hz sine wave)
//! - `frame_utils` - Shared frame helpers and conference port guard

// pub(super) so parent sip/ modules can access internal submodules directly
pub(super) mod direct_player;
pub(crate) mod frame_utils;
pub(super) mod init;
pub(super) mod looping_player;
pub(super) mod streaming_player;
pub(super) mod test_tone;
pub mod types;
pub(super) mod utils;

// Re-export public API for external consumers (crate::transport::sip::*)
pub use direct_player::*;
pub use init::*;
pub use looping_player::*;
pub use types::*;
