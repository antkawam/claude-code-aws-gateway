#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(data) {
        ccag::translate::request::sanitize_cache_control(&mut value);
    }
});
