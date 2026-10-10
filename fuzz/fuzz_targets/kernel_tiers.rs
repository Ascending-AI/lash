//! Differential kernel execution, initially interpreter versus interpreter.
//! The shared harness compares full observations under the same schedule and
//! park/terminal observations under different slices. A crash is minimized
//! into a conformance case naming the kernel rule it breaks (SC-DESIGN §9.2).
#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use lash_kernel_conformance::smith::{Smith, check_interpreter};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(smith) = Smith::arbitrary(&mut Unstructured::new(data)) {
        check_interpreter(&smith).unwrap_or_else(|error| {
            panic!("{error}\nprogram: {:#?}\nschedule: {:#?}", smith.document, smith.schedule)
        });
    }
});
