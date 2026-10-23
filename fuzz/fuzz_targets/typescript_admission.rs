//! FIG-7237: malformed and ambiguous TypeScript reaches lowering and admission.
#![no_main]

#[path = "../../crates/lash-dialect-typescript/shared/source_admission.rs"]
mod source_admission;

use libfuzzer_sys::fuzz_target;
use std::cell::RefCell;

thread_local! {
    static HARNESS: RefCell<source_admission::Harness> = RefCell::new(source_admission::Harness::new().unwrap_or_else(|error| panic!("source admission setup: {error}")));
}

fuzz_target!(|data: &[u8]| {
    HARNESS.with(|harness| harness.borrow_mut().check(data));
});
