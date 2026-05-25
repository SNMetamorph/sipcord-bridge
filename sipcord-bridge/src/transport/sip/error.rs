//! Typed error types for the SIP transport layer.
//!
//! Three sibling enums cover the three phases of the SIP path:
//!
//! - [`SipInitError`] — startup: pjsua create/init/start, transports, codecs,
//!   account registration. One-shot failures that take down the process.
//! - [`SipResponseError`] — building or sending an individual SIP response
//!   (401/302/200 etc.) from inside an FFI callback. Per-call, recoverable
//!   in the sense that we log and continue.
//! - [`SipAudioError`] — runtime audio plumbing: hooking players into a
//!   call's conference port. Surfaces when media isn't ready yet, when a
//!   port name has an interior NUL, or when pjsua refuses a connect.
//!
//! [`SipError`] is the umbrella for callers that want to handle any of them
//! uniformly. Conversion is via `#[from]`, so `?` propagation works through
//! the hierarchy without explicit `map_err`.

use std::ffi::NulError;

/// Umbrella error for everything the SIP transport layer can return.
#[derive(thiserror::Error, Debug)]
pub enum SipError {
    #[error(transparent)]
    Init(#[from] SipInitError),

    #[error(transparent)]
    Response(#[from] SipResponseError),

    #[error(transparent)]
    Audio(#[from] SipAudioError),

    #[error(transparent)]
    Call(#[from] SipCallError),
}

/// Errors raised by outbound-call setup (`make_outbound_call`).
#[derive(thiserror::Error, Debug)]
pub enum SipCallError {
    /// A URI / display-name string couldn't be converted to CString because
    /// of an interior NUL byte.
    #[error("invalid {field} for outbound call: {source}")]
    InvalidString {
        field: &'static str,
        #[source]
        source: NulError,
    },

    /// `pjsua_call_make_call` returned non-success.
    #[error("pjsua_call_make_call failed (status {0})")]
    MakeCall(i32),
}

/// Errors raised by `init_pjsua`, `create_tls_transport`, `reload_tls_transport`,
/// `process_pjsua_events`, and friends.
#[derive(thiserror::Error, Debug)]
pub enum SipInitError {
    /// A pjsua API returned a non-success status code. `operation` names the
    /// specific call (`"pjsua_create"`, `"pjsua_init"`, `"pjsua_start"`,
    /// `"pjsua_acc_add"`, `"pjsua_set_null_snd_dev"`, `"pjsua_handle_events"`,
    /// etc.).
    #[error("pjsua {operation} failed (status {status})")]
    Pjsua {
        operation: &'static str,
        status: i32,
    },

    /// `pjsua_transport_create` failed for `kind` ("UDP", "TCP", or "TLS").
    #[error("transport create ({kind}) failed (status {status})")]
    TransportCreate {
        kind: &'static str,
        status: i32,
    },

    /// A configuration string (host name, URI, etc.) couldn't be converted
    /// to a `CString` because of an interior NUL byte.
    #[error("invalid {field} string for FFI: {source}")]
    InvalidString {
        field: &'static str,
        #[source]
        source: NulError,
    },

    /// A `Path` to be passed into pjsua wasn't valid UTF-8.
    #[error("{field} path is not valid UTF-8")]
    NonUtf8Path { field: &'static str },
}

/// Errors raised by audio-port plumbing (`play_audio_to_call_direct`,
/// `start_loop`, `start_test_tone_to_call`, etc.) and the helpers in
/// `frame_utils`.
#[derive(thiserror::Error, Debug)]
pub enum SipAudioError {
    /// The call doesn't have a conference port yet — media negotiation is
    /// still in progress, or the call has just ended. Caller can retry or
    /// drop the audio.
    #[error("no conference port for call {call_id} (media not ready yet)")]
    NoConfPort { call_id: super::ffi::types::CallId },

    /// A port name (used to identify the player in pjsua's mixer) contains
    /// an interior NUL.
    #[error("invalid port name: {0}")]
    InvalidPortName(#[from] NulError),

    /// `pjsua_conf_add_port`, `pjsua_conf_connect`, etc. returned non-success.
    #[error("pjsua conf {operation} failed (status {status})")]
    Pjsua {
        operation: &'static str,
        status: i32,
    },

    /// Frame size / port count mismatch between the audio source and the
    /// pjsua port.
    #[error("frame mismatch: {0}")]
    FrameMismatch(String),

    /// Failure setting up a streaming player (file read, decoder, etc.).
    #[error(transparent)]
    Streaming(#[from] crate::services::sound::StreamingError),
}

/// Errors that can occur while building or sending a SIP response.
///
/// Surfaces failures from the pjsua/pjsip FFI surface — CString conversion,
/// pool allocation, header creation, and the final stateless / transactional
/// send. Variants stay coarse-grained because the typical caller is a pjsip
/// callback that can only log and continue.
#[derive(thiserror::Error, Debug)]
pub enum SipResponseError {
    /// A runtime string contained an interior NUL byte and could not be
    /// converted to `CString`.
    #[error("CString conversion failed (interior NUL)")]
    CStringNul(#[from] NulError),

    /// `pjsua_pool_create` returned null — out of memory or pjsua not
    /// initialised.
    #[error("pjsua pool allocation failed")]
    PoolAlloc,

    /// `pjsip_generic_string_hdr_create` returned null.
    #[error("pjsip header creation failed")]
    HeaderCreate,

    /// `pjsua_get_pjsip_endpt` returned null — pjsua not initialised.
    #[error("pjsip endpoint is null (pjsua not initialised)")]
    EndpointNull,

    /// `pjsip_endpt_respond_stateless` returned a non-success pj status code.
    #[error("pjsip stateless send failed (status {0})")]
    StatelessSend(i32),

    /// `pjsip_tsx_create_uas2` returned a non-success pj status code.
    #[error("pjsip UAS transaction creation failed (status {0})")]
    TsxCreate(i32),

    /// `pjsip_endpt_create_response` returned a non-success pj status code.
    #[error("pjsip response build failed (status {0})")]
    ResponseBuild(i32),

    /// `pjsua_call_answer` returned a non-success pj status code.
    #[error("pjsua_call_answer failed (status {0})")]
    CallAnswer(i32),
}
