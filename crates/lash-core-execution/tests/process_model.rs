//! Public process registry transition contracts, with their original unit-test names.

#![expect(
    clippy::expect_used,
    reason = "serialization fixture helpers assert that their setup is valid"
)]

mod runtime {
    mod process {
        mod registry_transitions;
    }
}
