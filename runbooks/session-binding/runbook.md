# Session binding runbook

Use this runbook when changing facade session storage selection, park/resume,
turn control, or session administration. The goal is to make noncoincident
owners visible: use different values for the exact root store, root catalog,
child catalog, and effect deployment so an accidental fallback cannot pass.

This is a developer checklist triggered by a diff, not a scenario a shard drives:
it names no scenario host, no prompt, no browser surface and no provider, and its
evidence is a list of commands. Read it that way.

## Fixture

The lettered fixture below is a **reading aid for the observations, not a fixture any
command here constructs**. Each regression test named under Evidence builds its own
equivalent; do not try to stand this up by hand and then run the tests against it, because
you will not reproduce the one they use.

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
4. These are two different drivers with two different outcomes; check both, and do not
   expect the second to behave like the first.
   - The **session-bound** driver over a revoked or omitted target gate: cancellation
     returns `UnknownOrRevoked`, performs zero catalog lookups, and writes no row.
   - The **catalog** (arbitrary-session) driver over a catalog that fails every lookup:
     it performs exactly **one** open and surfaces a typed `RuntimeStore` error. One open,
     not zero — the failure is reported after the open attempt, not instead of it.
5. Delete through an owner-issued native context. Inject a failure after the
   storage tombstone but before journal retirement, then retry using the same
   administration owner. The retry completes cleanup without reopening the
   session. Repeat through the Restate-installed administration inside a real
   handler; preserve its existing direct `DeleteSession` process command.
6. Retain a fork point, delete its source session, and fork from the retained
   point. The owning catalog still creates the destination and applies its own
   admission and fences.

## Evidence

Run `kiln build` for the workspace compile and `kiln clippy` for the workspace
lint gate. Then run the focused binding and deletion regressions.

Both live durable geometries — `just agent-workbench-restate-e2e` and
`just restate-postgres-workers-e2e` — are also required, but **not in one pass**: each
brings up its own compose stack, each costs tens of minutes on a cold fork, and they cannot
share a gate slot. Budget them as two serial runs.

Record the exact source SHA, command, exit code, and log path. This runbook has no
`LASH_*_ARTIFACT_DIR` of its own, so name a destination directory explicitly on the command
line and keep the pass record there with the run artifacts; otherwise the evidence is
whatever the runner happened to keep. A source-only grep or a coincident all-in-memory fixture is
not evidence for owner selection.
