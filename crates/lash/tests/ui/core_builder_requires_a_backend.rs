//! A core cannot be built without a backend: the builder takes the one
//! substrate every port comes from as a required argument, so there is no
//! build that falls back to an in-memory default (ADR 0102).

fn main() {
    let _ = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded);
}
