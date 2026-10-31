// SPDX-License-Identifier: AGPL-3.0-only

//! Sketchy edges view state over the FFI (itsjustcad-ios#174): the on/off
//! switch, `edgefx`-token tuning, and null-handle safety.

use std::ffi::{CStr, CString, c_char};

use itsjustcad_ffi::{
    ijc_free, ijc_get_sketchy_params, ijc_init_headless, ijc_set_sketchy, ijc_set_sketchy_params,
    ijc_sketchy_enabled, ijc_string_free,
};

fn text(p: *mut c_char) -> String {
    assert!(!p.is_null());
    let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
    unsafe { ijc_string_free(p) };
    s
}

#[test]
fn sketchy_toggle_round_trips() {
    let h = ijc_init_headless();
    unsafe {
        assert!(!ijc_sketchy_enabled(h), "off by default");
        assert!(ijc_set_sketchy(h, true));
        assert!(ijc_sketchy_enabled(h));
        assert!(text(ijc_get_sketchy_params(h)).starts_with("enabled=1 "));
        assert!(ijc_set_sketchy(h, false));
        assert!(!ijc_sketchy_enabled(h));
        ijc_free(h);
    }
}

#[test]
fn sketchy_params_tune_without_switching_on() {
    let h = ijc_init_headless();
    let tokens =
        CString::new("jitter=0.1 passes=3 ext=0.2 depthcue=0.5 endpoints=1 bogus=9").unwrap();
    unsafe {
        assert!(ijc_set_sketchy_params(h, tokens.as_ptr()));
        assert!(!ijc_sketchy_enabled(h), "tuning leaves the switch alone");
        let p = text(ijc_get_sketchy_params(h));
        assert_eq!(
            p,
            "enabled=0 jitter=0.1 passes=3 extension=0.2 depthcue=0.5 endpoints=1 hatching=0"
        );
        // The getter's tokens round-trip through the setter.
        let again = CString::new(p.clone()).unwrap();
        assert!(ijc_set_sketchy_params(h, again.as_ptr()));
        assert_eq!(text(ijc_get_sketchy_params(h)), p);
        assert!(!ijc_set_sketchy_params(h, std::ptr::null()));
        ijc_free(h);
    }
}

#[test]
fn sketchy_calls_are_null_safe() {
    let null = std::ptr::null_mut();
    let tokens = CString::new("jitter=0.1").unwrap();
    unsafe {
        assert!(!ijc_set_sketchy(null, true));
        assert!(!ijc_sketchy_enabled(null));
        assert!(!ijc_set_sketchy_params(null, tokens.as_ptr()));
        assert!(ijc_get_sketchy_params(null).is_null());
    }
}
