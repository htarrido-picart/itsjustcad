// SPDX-License-Identifier: AGPL-3.0-only

//! The Hidden Line and Sketchy render styles over the FFI
//! (itsjustcad-ios#174): mode round trip, `edgefx`-token tuning, pixel scale,
//! and null-handle safety.

use std::ffi::{CStr, CString, c_char};

use itsjustcad_ffi::{
    ijc_free, ijc_get_render_mode, ijc_get_sketchy_params, ijc_init_headless, ijc_set_pixel_scale,
    ijc_set_render_mode, ijc_set_sketchy_params, ijc_string_free,
};

fn text(p: *mut c_char) -> String {
    assert!(!p.is_null());
    let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
    unsafe { ijc_string_free(p) };
    s
}

fn set_mode(h: *mut itsjustcad_ffi::AppHandle, mode: &str) -> bool {
    let mode = CString::new(mode).unwrap();
    unsafe { ijc_set_render_mode(h, mode.as_ptr()) }
}

#[test]
fn hidden_line_and_sketchy_modes_round_trip() {
    let h = ijc_init_headless();
    for (token, label) in [
        ("hiddenline", "Hidden Line"),
        ("sketchy", "Sketchy"),
        ("shaded", "Shaded"),
    ] {
        assert!(set_mode(h, token), "{token}");
        assert_eq!(text(unsafe { ijc_get_render_mode(h) }), label);
    }
    unsafe { ijc_free(h) };
}

#[test]
fn sketchy_params_round_trip() {
    let h = ijc_init_headless();
    let tokens =
        CString::new("jitter=0.1 passes=3 ext=0.2 depthcue=0.5 endpoints=1 bogus=9").unwrap();
    unsafe {
        assert!(ijc_set_sketchy_params(h, tokens.as_ptr()));
        let p = text(ijc_get_sketchy_params(h));
        assert_eq!(
            p,
            "jitter=0.1 passes=3 extension=0.2 depthcue=0.5 endpoints=1 hatching=0"
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
fn pixel_scale_rejects_bad_values() {
    let h = ijc_init_headless();
    unsafe {
        assert!(ijc_set_pixel_scale(h, 3.0));
        assert!(!ijc_set_pixel_scale(h, 0.0));
        assert!(!ijc_set_pixel_scale(h, f32::NAN));
        ijc_free(h);
    }
}

#[test]
fn calls_are_null_safe() {
    let null = std::ptr::null_mut();
    let tokens = CString::new("jitter=0.1").unwrap();
    unsafe {
        assert!(!set_mode(null, "sketchy"));
        assert!(!ijc_set_sketchy_params(null, tokens.as_ptr()));
        assert!(ijc_get_sketchy_params(null).is_null());
        assert!(!ijc_set_pixel_scale(null, 2.0));
    }
}
