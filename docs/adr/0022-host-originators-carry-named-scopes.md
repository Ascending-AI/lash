# Host originators carry named scopes

## Decision

`ProcessOriginator::Host` carries an optional opaque scope: `Host { scope: Option<String> }`. Its projected identity is `host` when absent and `host:{scope}` when present. Trigger subscription listing and lifecycle operations match registrant scope uniformly for Host and Session originators.

## Why and consequences

Hosts choose grouping labels such as automation ids or CLI profiles. Lash assigns no product meaning or security policy to them. A single host bucket is insufficient for independent lifecycle groups. Synthetic session ids are rejected because they misstate provenance and require downstream readers to interpret that fiction.

Absent scope preserves the unscoped host representation. [Originator construction and projection](../../crates/lash-core-execution/src/runtime/process/model.rs) own the identity; [trigger registrants](../../crates/lash-core-execution/src/triggers.rs) use it for scope matching.
