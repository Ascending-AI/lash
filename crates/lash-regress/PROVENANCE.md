# Provenance

`lash-regress` is a fork of the `regress` crate version 0.11.1:

- Upstream repository: <https://github.com/ridiculousfish/regress>
- Upstream commit: `7e64ad5e6807b5503e5cc97a79e0f129b23c556b`
- Upstream author: ridiculous_fish (Cory Doras)
- Local purpose: deterministic fuel/step-budget instrumentation and the
  anchored-matching API used by Lashlang

The upstream `LICENSE-MIT` and `LICENSE-APACHE` files are preserved verbatim.
Upstream copyright and modification notices remain in the carried source and
test files. The package and Rust crate were renamed from `regress` to
`lash-regress` and `lash_regress`, respectively, so downstream Lash consumers
resolve the maintained fork without a Cargo patch.
