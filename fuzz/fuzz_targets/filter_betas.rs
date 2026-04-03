#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    ccag::translate::models::filter_betas(data);
});
