# A linked dataframe library

`df.ts` is ordinary TypeScript code. It exports `load` and `to_cols`; it has
no main and no custom package surface. The source importer produces a Document
with typed Imports and Exports. The host fulfils its Imports once and publishes
an ordinary `df` Package pointing at that Link.

`cell.ts` imports the library from a cell. `nightly.ts` imports the same functions
from a workflow. Library calls, loops and callbacks stay inside the VM. Only
`io.read_csv` and the ordinary TypeScript clock operation cross the host boundary.
The example's IO provider reads an explicit host map of authorized CSV paths.

`host::build` takes a backend, node owner, existing artifact pin and CSV map.
The caller owns pin cleanup before any publication await. Process components
advertise IO and dialect operations; the host records their exact publication
provenance, publishes the library, and retains its Link in the namespace.
`WorkflowCreator::create` publishes a workflow's selected Export;
`WorkflowCreator::start` selects routes by OperationUse and starts it through the
normal durable process API. A new namespace never relinks already published code.

After `. ./env.sh`, run the library boundary law on SQLite:

```sh
kiln test //examples/dataframe:dataframe__unit_test --test_arg=tests::df_library_and_workflow_keep_their_defining_links --test_arg=--exact
```
