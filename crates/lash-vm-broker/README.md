# lash-vm-broker

The parent side of the worker boundary (ADR 0123): it binds a worker's run to
the parent's admitted execution context, authorises and dispatches every
effect the worker requests, issues journal ordinals and `ToolCallId`s,
commits checkpoints atomically, and recovers a lost worker through the
substrate. It carries no transport and no pool; it defines the seams they
implement.
