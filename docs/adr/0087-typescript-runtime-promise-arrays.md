# 0087: TypeScript aggregates evaluate runtime arrays

## Status

Replaced by [ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md) §11.

## Note

TypeScript Promise helpers operate over kernel tasks and list joins. Runtime arrays and duplicate positions preserve source order; a process handle is an ordinary value until explicitly awaited through its tool.
