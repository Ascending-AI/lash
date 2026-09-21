//! The rebind-completeness guard (FIG-3429 item 2).
//!
//! [`REBIND_FIELDS`] is the contract a tool-child driver answers to when it
//! turns a lent opener context into a child's context: every field of
//! [`ToolDispatchContext`] carries exactly one ruling — rebound from the
//! recorded request, lent from the live opener, or fresh for the child. A field
//! added to the struct without a ruling here is a field some driver will
//! forget, and a forgotten ruling is an authority leak no scenario test has to
//! exist for. This test is the proof the list cannot drift: it reads the
//! struct's own declaration and refuses any field the list does not name, in
//! either direction.

use std::collections::BTreeSet;

use super::context_source::dispatch_context_fields;
use crate::tool_dispatch::REBIND_FIELDS;

#[test]
fn every_dispatch_context_field_has_a_rebind_ruling() {
    let fields = dispatch_context_fields();
    let ruled: BTreeSet<&'static str> = REBIND_FIELDS
        .iter()
        .map(|field| field.context_field())
        .collect();
    let unruled: Vec<&String> = fields
        .iter()
        .filter(|field| !ruled.contains(field.as_str()))
        .collect();
    assert!(
        unruled.is_empty(),
        "ToolDispatchContext fields with no REBIND_FIELDS ruling: {unruled:?}; \
         add each to RebindField, give it a disposition, and bump \
         TOOL_CHILD_REBIND_VERSION"
    );
}

#[test]
fn every_ruling_names_a_real_dispatch_context_field() {
    let declared = dispatch_context_fields();
    let fields: BTreeSet<&str> = declared.iter().map(String::as_str).collect();
    let stray: Vec<&'static str> = REBIND_FIELDS
        .iter()
        .map(|field| field.context_field())
        .filter(|name| !fields.contains(name))
        .collect();
    assert!(
        stray.is_empty(),
        "REBIND_FIELDS names fields ToolDispatchContext does not have: {stray:?}"
    );
}

#[test]
fn rebind_fields_lists_each_field_once() {
    let unique: BTreeSet<_> = REBIND_FIELDS.iter().collect();
    assert_eq!(
        unique.len(),
        REBIND_FIELDS.len(),
        "REBIND_FIELDS lists a field twice"
    );
    let names: BTreeSet<_> = REBIND_FIELDS
        .iter()
        .map(|field| field.context_field())
        .collect();
    assert_eq!(
        names.len(),
        REBIND_FIELDS.len(),
        "two rulings name the same context field"
    );
}
