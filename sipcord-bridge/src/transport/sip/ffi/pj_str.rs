//! Safe(r) helpers around the pjsua/pjsip C-string and header-building idioms
//! that recur across the SIP transport layer.
//!
//! Before this module existed, every callback that built a SIP header
//! re-implemented the same pattern:
//!
//! ```ignore
//! let name = CString::new("Contact").unwrap();
//! let value = CString::new(runtime_str).unwrap();
//! let name_pj = pj_str(name.as_ptr() as *mut c_char);
//! let value_pj = pj_str(value.as_ptr() as *mut c_char);
//! let hdr = pjsip_generic_string_hdr_create(pool, &name_pj, &value_pj);
//! // ...
//! ```
//!
//! That sprouted two unwraps per header (so any header value containing a NUL
//! byte from upstream data would panic), repeated lifetime traps, and zero
//! shared failure handling. The helpers in this module turn those calls into
//! a single fallible call returning [`SipResponseError`].

use crate::transport::sip::error::SipResponseError;
use pjsua::*;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;

/// Convert a [`CStr`] (typically a `c"..."` literal) into a [`pj_str_t`].
///
/// Zero-cost — `pj_str` just wraps the pointer and length. The caller must
/// keep the `CStr` alive for the `pj_str_t`'s usage window. For `&'static`
/// literals (the common case) that's trivially satisfied.
#[inline]
pub unsafe fn pj_str_from_cstr(s: &CStr) -> pj_str_t {
    unsafe { pj_str(s.as_ptr() as *mut c_char) }
}

// A `pj_str_owned(&str) -> Result<(CString, pj_str_t), _>` helper was considered
// but turned out unused: every runtime-string call site in this codebase ends
// up either inside `make_string_hdr` (which does the conversion internally) or
// in a function that already chains `CString::new(...).context("...")?` for a
// site-specific error message. Add it back if a true caller appears.

/// Initialise a `pjsip_hdr` as an empty list head (equivalent to the
/// `pj_list_init` C macro).
#[inline]
pub unsafe fn pj_list_init_hdr(hdr: *mut pjsip_hdr) {
    unsafe {
        (*hdr).next = hdr as *mut _;
        (*hdr).prev = hdr as *mut _;
    }
}

/// Create a generic string header in `pool`.
///
/// `name` is a static `CStr` (use `c"Contact"` etc); `value` is a runtime
/// string that gets converted to a `CString` and duplicated into the pool
/// by pjsip. The temporary `CString` is dropped before this returns;
/// `pjsip_generic_string_hdr_create` uses `pj_strdup` internally to copy
/// the bytes.
pub unsafe fn make_string_hdr(
    pool: *mut pj_pool_t,
    name: &CStr,
    value: &str,
) -> Result<*mut pjsip_generic_string_hdr, SipResponseError> {
    unsafe {
        let value_c = CString::new(value)?;
        let name_pj = pj_str_from_cstr(name);
        let value_pj = pj_str(value_c.as_ptr() as *mut c_char);
        let hdr = pjsip_generic_string_hdr_create(pool, &name_pj, &value_pj);
        if hdr.is_null() {
            return Err(SipResponseError::HeaderCreate);
        }
        Ok(hdr)
    }
}

/// Append a generic string header onto the message buffer in `tdata`,
/// allocating from the tdata's own pool.
pub unsafe fn append_tdata_hdr(
    tdata: *mut pjsip_tx_data,
    name: &CStr,
    value: &str,
) -> Result<(), SipResponseError> {
    unsafe {
        let hdr = make_string_hdr((*tdata).pool, name, value)?;
        pj_list_insert_before(
            &mut (*(*tdata).msg).hdr as *mut pjsip_hdr as *mut pj_list_type,
            hdr as *mut pj_list_type,
        );
        Ok(())
    }
}

/// Answer a pjsua call with N custom headers attached to the response.
///
/// Wraps the recurring `pjsua_msg_data_init` + pool + header build +
/// `pjsua_call_answer` dance used in 401 / 302 / 4xx code paths.
///
/// **The pool is intentionally NOT released.** pjsua may continue to reference
/// the header data after `pjsua_call_answer` returns; releasing the pool here
/// triggers use-after-free. Each call leaks ~512 bytes that's reclaimed when
/// pjsua shuts down. (Tracking pools per-call and releasing them on call-end
/// would be a cleaner fix; not in scope here.)
///
/// On error, the caller typically follows up with `pjsua_call_hangup` — this
/// helper does not hang up on its own so the caller can choose the strategy.
pub unsafe fn answer_call_with_headers(
    call_id: i32,
    status_code: u32,
    reason: &CStr,
    pool_name: &CStr,
    headers: &[(&CStr, &str)],
) -> Result<(), SipResponseError> {
    unsafe {
        let mut msg_data = std::mem::MaybeUninit::<pjsua_msg_data>::uninit();
        pjsua_msg_data_init(msg_data.as_mut_ptr());
        let msg_data_ptr = msg_data.assume_init_mut();

        let pool = pjsua_pool_create(pool_name.as_ptr(), 512, 512);
        if pool.is_null() {
            return Err(SipResponseError::PoolAlloc);
        }
        // Intentionally leaked — see doc comment above.

        for (name, value) in headers {
            let hdr = make_string_hdr(pool, name, value)?;
            pj_list_insert_before(
                &mut msg_data_ptr.hdr_list as *mut _ as *mut pj_list_type,
                hdr as *mut pj_list_type,
            );
        }

        let reason_pj = pj_str_from_cstr(reason);
        let status = pjsua_call_answer(call_id, status_code, &reason_pj, msg_data_ptr);
        if status != pj_constants__PJ_SUCCESS as i32 {
            return Err(SipResponseError::CallAnswer(status));
        }
        Ok(())
    }
}

/// Send a stateless SIP response with N string headers.
///
/// Wraps the recurring `pjsua_pool_create` → list-head alloc → header
/// build → `pjsip_endpt_respond_stateless` → `pj_pool_release` dance. Each
/// header in `headers` is a `(name, value)` pair where `name` is typically
/// a `c"..."` literal and `value` is any runtime string.
///
/// `reason` is the SIP reason phrase (e.g. `Some(c"Unauthorized")`) or
/// `None` to let pjsip pick the default for `status_code`.
pub unsafe fn respond_stateless_with_headers(
    rdata: *mut pjsip_rx_data,
    status_code: u16,
    reason: Option<&CStr>,
    headers: &[(&CStr, &str)],
) -> Result<(), SipResponseError> {
    unsafe {
        let endpt = pjsua_get_pjsip_endpt();
        if endpt.is_null() {
            return Err(SipResponseError::EndpointNull);
        }

        let pool = pjsua_pool_create(c"sip_resp".as_ptr(), 1024, 1024);
        if pool.is_null() {
            return Err(SipResponseError::PoolAlloc);
        }

        // Belt-and-braces: ensure the pool is released even if a step
        // between here and the send returns Err via `?`.
        let result =
            (|| -> Result<i32, SipResponseError> {
                let hdr_list =
                    pj_pool_alloc(pool, std::mem::size_of::<pjsip_hdr>()) as *mut pjsip_hdr;
                if hdr_list.is_null() {
                    return Err(SipResponseError::PoolAlloc);
                }
                pj_list_init_hdr(hdr_list);

                for (name, value) in headers {
                    let hdr = make_string_hdr(pool, name, value)?;
                    pj_list_insert_before(
                        hdr_list as *mut pj_list_type,
                        hdr as *mut pj_list_type,
                    );
                }

                let reason_pj = reason.map(|r| pj_str_from_cstr(r));
                let reason_ptr = reason_pj
                    .as_ref()
                    .map(|r| r as *const pj_str_t)
                    .unwrap_or(ptr::null());

                Ok(pjsip_endpt_respond_stateless(
                    endpt,
                    rdata,
                    status_code.into(),
                    reason_ptr,
                    hdr_list,
                    ptr::null(),
                ))
            })();

        pj_pool_release(pool);

        match result {
            Ok(status) if status == pj_constants__PJ_SUCCESS as i32 => Ok(()),
            Ok(status) => Err(SipResponseError::StatelessSend(status)),
            Err(e) => Err(e),
        }
    }
}
