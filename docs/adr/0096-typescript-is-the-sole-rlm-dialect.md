# 0096: One IR and VM, extensible dialects, TypeScript today

## Status

Replaced by [ADR 0139](0139-the-lash-vm-is-a-dialect-free-kernel.md). Code on
main still cites this file.

## Note

This decision kept one IR and VM for every dialect, with TypeScript the only
one shipped. The kernel is that one language, TypeScript and Python both lower
to it (ADR 0139, "Dialects lower to the kernel"), and [ADR
0138](0138-codemode-cells-are-kernel-programs.md) owns how a session selects its
dialect.
