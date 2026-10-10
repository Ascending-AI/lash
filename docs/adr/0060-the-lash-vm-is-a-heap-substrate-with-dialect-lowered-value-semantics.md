# The Lash VM is a heap substrate with dialect-lowered value semantics

## Status

Replaced by [ADR 0139](0139-the-lash-vm-is-a-dialect-free-kernel.md). Code on
main still cites this file.

## Note

This decision made the VM a heap of ECMAScript-shaped objects that each source
dialect lowered its value semantics into. The kernel has its own values, with
identity for lists, maps, sets and records and closures that capture by
reference, and every dialect compiles its differences into kernel code (ADR
0139, "One kernel, no source language").
