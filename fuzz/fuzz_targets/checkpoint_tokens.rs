//! Direct, bounded malformed-token admission, without an envelope/hash prefilter.
#![no_main]

use lash_kernel_doc::Datum;
use lash_kernel_state::paged::{Checkpoint, CheckpointRoot};
use {lash_kernel_value::tokens::TokenCodec, lash_kernel_value::tokens::decode_body_with_limits, lash_kernel_value::tokens::encode_body};
use libfuzzer_sys::fuzz_target;

fn canonical<T: TokenCodec>(bytes: &[u8]) {
    // Narrow recursion independently of the input byte cap, including recursive
    // function captures. The standard-depth refusal already has its own law.
    let limits = lash_kernel_value::KernelDataLimits::new(64, 352, 64, 512, 128).unwrap();
    if let Ok(value) = decode_body_with_limits::<T>(bytes, limits) {
        assert_eq!(encode_body(&value), bytes, "accepted a second spelling");
    }
}

fuzz_target!(|bytes: &[u8]| {
    if bytes.len() > 4096 {
        return;
    }
    canonical::<Datum>(bytes);
    canonical::<CheckpointRoot>(bytes);
    canonical::<Checkpoint>(bytes);
});
