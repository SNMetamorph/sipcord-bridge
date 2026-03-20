//! Ban system trait definition
//!
//! The trait is defined here so FFI callbacks in the library can call ban checks.
//! When no implementation is registered (e.g. standalone/static-router mode),
//! ban checks are simply skipped.

use std::net::IpAddr;
use std::sync::{Arc, OnceLock};

/// Result of checking/recording a ban
#[derive(Debug, Clone, Copy)]
pub struct BanCheckResult {
    /// Current offense level for this IP (progressive timeout key)
    pub offense_level: u32,
    /// Whether the IP is currently timed out or banned
    pub is_banned: bool,
    /// Whether this is a permanent ban (vs progressive timeout)
    pub is_permanent: bool,
    /// Timeout duration in seconds (0 if not timed out)
    pub timeout_secs: u64,
    /// Whether we should log this attempt
    pub should_log: bool,
}

/// Result of clearing all ban data
#[derive(Debug)]
pub struct ClearResult {
    pub bans_cleared: u64,
    pub registers_cleared: u64,
}

/// Trait for ban checking — implemented by the adapter, consumed by FFI callbacks
pub trait BanCheck: Send + Sync {
    fn is_enabled(&self) -> bool;
    fn is_whitelisted(&self, ip: &IpAddr) -> bool;
    fn check_banned(&self, ip: &IpAddr) -> BanCheckResult;
    fn record_offense(&self, ip: IpAddr, reason: &str) -> BanCheckResult;
    fn record_permanent_ban(&self, ip: IpAddr, reason: &str) -> BanCheckResult;
    /// Record a REGISTER request from an IP. Returns true if rate limited.
    fn record_register(&self, ip: IpAddr) -> bool;
    fn clear_all(&self) -> Result<ClearResult, Box<dyn std::error::Error + Send + Sync>>;
    /// Config accessors for extension-length checks in callbacks
    fn suspicious_extension_min_length(&self) -> usize;
    fn suspicious_extension_max_length(&self) -> usize;
    fn permaban_extension_min_length(&self) -> usize;
}

static GLOBAL_BAN_CHECK: OnceLock<Arc<dyn BanCheck>> = OnceLock::new();

/// Register a global ban checker (called by the adapter at init time)
pub fn set_global(checker: Arc<dyn BanCheck>) {
    let _ = GLOBAL_BAN_CHECK.set(checker);
}

/// Get the global ban checker (None if not registered)
pub fn global() -> Option<&'static Arc<dyn BanCheck>> {
    GLOBAL_BAN_CHECK.get()
}
