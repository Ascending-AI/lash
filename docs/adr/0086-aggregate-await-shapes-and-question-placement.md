# 0086: Aggregate await operates on handles

## Status

Replaced by [ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md) §10 and §11.

## Note

Kernel aggregates join task handles; process handles remain data. A process wait is an explicit `processes.await` operation. The kernel has no recursive process-handle-container await.
