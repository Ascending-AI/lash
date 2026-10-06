# Tool-call directives compose monotonically

## Status

Replaced by [ADR 0128](0128-tool-hooks-compose-as-transforms-then-checks.md),
which owns the 1.0 hook composition contract. A script fixture still names
this file; the ADR 0128 lanes delete it.

## Note

This decision folded mixed before- and after-tool directives with a
restrictive terminal ordering and bounded reinspection. ADR 0128 replaces the
mixed directives with transforms followed by checks.
