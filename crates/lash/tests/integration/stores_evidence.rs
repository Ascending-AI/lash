//! Compile-time witnesses for store conformance support contracts.
//!
//! These probes type-check public contracts without constructing a live backend.

#![cfg(feature = "testing")]
#![allow(dead_code, unreachable_code, unused_variables)]

fn type_witness<T>() {}
fn member_witness<T>(_: T) {}
fn field_witness<T>(_: impl FnOnce(&T)) {}
fn variant_witness<T>(_: impl FnOnce(&T) -> bool) {}
