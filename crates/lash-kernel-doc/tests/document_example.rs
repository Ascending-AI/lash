//! The kernel-text example in [`parse_document`]'s rustdoc is
//! `src/text/document_example.rs` verbatim, through `#[doc = include_str!]`;
//! `include!` runs it here. The workspace builds no doctests, so this test
//! is the doc's compile-and-run proof.
//!
//! [`parse_document`]: lash_kernel_doc::parse_document
#![expect(
    clippy::expect_used,
    reason = "test target: the documented example fails by panicking"
)]

include!("../src/text/document_example.rs");

#[test]
fn the_parse_document_docs_example_runs() {
    document_example();
}
