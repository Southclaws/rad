#![no_main]

use libfuzzer_sys::fuzz_target;
use rad::protocol::{generated::pir, lower_pir};

fuzz_target!(|input: &[u8]| {
    if input.len() > 256 * 1024 {
        return;
    }
    let Ok(program) = serde_json::from_slice::<pir::Program>(input) else {
        return;
    };
    let _ = lower_pir(program);
});

