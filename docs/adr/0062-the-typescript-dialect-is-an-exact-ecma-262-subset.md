# The TypeScript dialect is an exact ECMA-262 subset

## Status

Replaced by [ADR 0139](0139-the-lash-vm-is-a-dialect-free-kernel.md). Code on
main still cites this file.

## Note

This decision held the TypeScript dialect to exact ECMA-262 meaning for every
accepted construct. The TypeScript dialect on the kernel trusts declared and
inferred types and raises a typed error where JavaScript would coerce; each
such difference is a row of its deviation register, and Test262 holds the rest
(ADR 0139, "The TypeScript dialect").
