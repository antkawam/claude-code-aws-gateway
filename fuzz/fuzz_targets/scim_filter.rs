#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let _ = ccag::scim::filter::parse_filter(data);
});
