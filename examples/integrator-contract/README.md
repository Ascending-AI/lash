# Certifying a store integration

This unpublished example implements `StoreSet` against the public `lash`
facade in `src/stores.rs`. Its production dependency is `lash`; certification
adds `lash-conformance`, a concrete SQLite substrate, and Tokio as
dev-dependencies. The conformance library is unpublished too: vendor it from
the same Lash revision as the store traits.

`src/lib.rs` and `src/storage.rs` also keep signature witnesses for the other
integrator traits. Their `unreachable!()` bodies check that the facade supplies
the complete vocabulary. Certification executes the store set in `src/stores.rs`.

`tests/conformance.rs` imports the complete runtime-persistence and
process-registry catalogues, including their reopen laws, plus the session
catalog, read-view, graph-append, retention, and attachment-store suites.
Registration macros create named tests and pick up new laws when the library
changes. The example neither copies assertions nor selects a smaller catalogue.
These certify storage contracts; engine crash recovery and live replay have
their own suites, described in `docs/agents/test-backends.md`.

To adapt the pattern to a third-party backend:

1. Implement the `StoreSet` ports in `IntegratorStores` over your database.
   All ports must share a substrate, transaction boundary, binding identity,
   and clock. This example delegates those ports to SQLite.
2. Replace `Fixture::open` and `Fixture::reopen` with your backend's constructors.
   Opening creates a fresh empty substrate; reopening creates distinct handles
   on the substrate already written. Retain every fixture until its law ends.
3. Supply `ConformanceDeployment` and `ConformanceProcessRegistry` views of the
   same ports for test-only mutation and inspection. Here those views come from
   the delegated SQLite implementation. Enable its `testing` feature only in
   the dev-dependency. Production ports require no conformance hooks.
4. Admit the named session before handing out runtime-persistence handles and
   publish the fixture environments before process registration. Give the
   persistence laws a controlled clock; attachment freshness needs real time.
5. Keep the imported registration macros. Run their named tests and inspect
   the executed count: compiling a mount or executing zero tests certifies
   nothing.

For a focused law in this checkout, source `env.sh` at the repository root:

```sh
kiln test //examples/integrator-contract:conformance__test \
  --test_arg=persistence::append_receipt_reopen --test_arg=--exact
```

Select each registered test's full path for a complete certification run.
The example uses SQLite memory, with SQL transactions and constraints.
Attachment persistence is `Ephemeral` because its bytes live only as long as
the named memory database; reopen still has to recover them while it is alive.
For a persistent substrate, declare `Durable` instead.
