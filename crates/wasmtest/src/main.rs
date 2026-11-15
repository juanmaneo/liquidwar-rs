//! WASM harness for the lwcore scenario suite.
//!
//! Compiled to `wasm32-unknown-unknown` and driven by `tools/wasm-test.cjs`
//! under Node. The host calls [`run_tests`] once, then reads the failure
//! list through the exported pointer/length pair. This proves the core
//! (including the lockstep protocol logic) is portable and deterministic
//! on a second platform, with no OS assumptions.

use std::sync::Mutex;

use lwcore::tests;

static FAILURES: Mutex<Option<Vec<u8>>> = Mutex::new(None);

/// Run the full scenario suite; results are stored for the host to read.
#[no_mangle]
pub extern "C" fn run_tests() {
    let failures = tests::run_tests();
    let mut text = failures.join("\n").into_bytes();
    if text.is_empty() {
        text = b"ALL SCENARIOS PASSED".to_vec();
    }
    *FAILURES.lock().unwrap() = Some(text);
}

/// Pointer to the failure-report buffer (valid until the next run_tests).
#[no_mangle]
pub extern "C" fn failures_ptr() -> *const u8 {
    match FAILURES.lock().unwrap().as_ref() {
        Some(v) if !v.is_empty() => v.as_ptr(),
        _ => std::ptr::null(),
    }
}

/// Length of the failure-report buffer in bytes.
#[no_mangle]
pub extern "C" fn failures_len() -> usize {
    FAILURES.lock().unwrap().as_ref().map_or(0, |v| v.len())
}

fn main() {
    // The wasm host calls run_tests through its export; a native build is
    // only for smoke-testing the harness itself.
    run_tests();
    let len = failures_len();
    if len > 0 {
        let ptr = failures_ptr();
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        let text = String::from_utf8_lossy(bytes);
        if text == "ALL SCENARIOS PASSED" {
            println!("wasmtest: {text}");
        } else {
            eprintln!("wasmtest failures:\n{text}");
            std::process::exit(1);
        }
    }
}
