# 0095: Processes are values, process controls are tools, one handle kind

## Status

Replaced by [ADR 0139](0139-the-lash-vm-is-a-dialect-free-kernel.md). Code on
main still cites this file.

## Note

This decision made a process definition a `Process` value lifted from a source
literal. The kernel has no process form: a document lists its entries, and
`processes.start` takes a function reference to one. Definition identity, the
one handle kind and start identity are owned by ADR 0139, "Processes are
entries started by effects".
