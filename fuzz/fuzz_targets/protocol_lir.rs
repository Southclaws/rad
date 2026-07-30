#![no_main]

use libfuzzer_sys::fuzz_target;
use rad::protocol::{generated::lir, lower_lir};

fuzz_target!(|input: &[u8]| {
    if input.len() > 256 * 1024 {
        return;
    }
    let Ok(query) = serde_json::from_slice::<lir::Query>(input) else {
        return;
    };
    let _ = lower_lir(query);
});

