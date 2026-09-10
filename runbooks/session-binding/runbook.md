# Session binding runbook

Use this runbook when changing facade session storage selection, park/resume,
turn control, or session administration. The goal is to make noncoincident
owners visible: use different values for the exact root store, root catalog,
child catalog, and effect deployment so an accidental fallback cannot pass.

## Fixture

Configure a core with catalog **B**, child catalog **C**, and control deployment
**control-B**. Open root session `binding-root` with an explicit store **A** and
run one accepted turn. Record each catalog open/create count, each store's
cancel rows, and each control deployment's gate resolutions.

## Required observations

1. Read, enqueue, cancel, park, resume, and close `binding-root`. Every store
   operation lands in **A** and every gate operation lands in the control owner
   captured when the session opened. Resuming through a core with different
   live provider or plugin configuration keeps those lifecycle owners.
2. Create a managed child. Its store comes from **C**. It never aliases **A**
   and never falls back to **B**.
3. Use the core's arbitrary-session driver against a session in **B**. It opens
   the target once, then retains that handle through cancellation recording and
   receipt readback.
4. Revoke or omit the target gate and make **B** fail every lookup. Cancellation
   returns `UnknownOrRevoked`, performs zero catalog lookups, and writes no row.
5. Build a core without a catalog. Opening without an explicit store returns
   `MissingSessionStore`; catalog and administration operations return
   `SessionCatalogUnavailable` before process, trigger, revocation, storage, or
   retirement effects.
6. Delete through an owner-issued native context. Inject a failure after the
   storage tombstone but before journal retirement, then retry using the same
   administration owner. The retry completes cleanup without reopening the
   session. Repeat through the Restate-installed administration inside a real
   handler; preserve its existing direct `DeleteSession` process command.
7. Retain a fork point, delete its source session, and fork from the retained
   point. The owning catalog still creates the destination and applies its own
   admission and fences.

## Evidence

Run workspace `check` and `clippy` with `--workspace --all-targets`, the focused
binding and deletion regressions, and both live durable geometries:
`just agent-workbench-restate-e2e` and
`just restate-postgres-workers-e2e`. Record the exact source SHA, command, exit
code, and log path. A source-only grep or a coincident all-in-memory fixture is
not evidence for owner selection.
