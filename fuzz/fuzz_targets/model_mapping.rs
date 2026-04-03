#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    ccag::translate::models::strip_date_suffix(data);
    ccag::translate::models::anthropic_to_bedrock(data, "us.", None);
    ccag::translate::models::bedrock_to_anthropic(data, None);
});
