#![no_main]

use libfuzzer_sys::fuzz_target;
use rad::engine::catalog::schema;

fuzz_target!(|input: &[u8]| {
    if input.len() > 256 * 1024 {
        return;
    }
    let _ = schema::parse("fuzz.schema.yaml", input);
});

