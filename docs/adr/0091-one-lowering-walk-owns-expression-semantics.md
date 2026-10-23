# 0091: One lowering walk owns expression semantics

## Status

Replaced by [ADR 0139](0139-the-lash-vm-is-a-dialect-free-kernel.md).

## Note

The kernel checker derives facts from one Document; source lowering belongs to each dialect. The expression linker, `Binding` inference and IR observer walk do not define the kernel checker or workflow view.
