//! Public durable effect contracts, with their original unit-test names.

#![expect(
    clippy::expect_used,
    reason = "contract fixtures assert that their setup is valid"
)]

mod runtime {
    mod effect {
        mod envelope;
        mod group;
        mod tool_child;
    }
}
