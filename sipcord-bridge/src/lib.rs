//! Sipcord Bridge - SIP to Discord Voice Bridge
//!
//! A generic SIP-to-Discord voice bridge library. Provides all the core
//! functionality for bridging SIP phone calls to Discord voice channels,
//! including fax (G.711 and T.38) support.
//!
//! Backends implement the `routing::Backend` trait to control call routing
//! and authentication. A built-in `StaticBackend` (TOML dialplan) is included.

#![feature(portable_simd)]
// Lock down the no-unwrap policy. Test modules opt out via the
// `#[cfg_attr(test, allow(...))]` shim at their boundary (or `#[allow]` at
// the test fn level for isolated cases). See feedback memories
// `feedback-no-unwrap-in-production` and `feedback-fix-clippy-at-source`.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod audio;
pub mod call;
pub mod config;
pub mod error;
pub mod fax;
pub mod routing;
pub mod services;
pub mod transport;

pub use error::BridgeError;
