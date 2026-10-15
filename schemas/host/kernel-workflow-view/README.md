# Workflow view host types

`v1.schema.json` describes the JSON returned by `lash::workflow::view::derive`.
`v1.d.ts` exports `WorkflowView` and every nested type from that schema, plus
`WorkflowCoverage`, `StatementCoverage` and `ScopeCoverage`: the JSON
`lash::workflow::view::coverage` returns. Hosts can
copy the declaration into their source tree and import its types directly:

```typescript
import type { WorkflowView, ActionView, Use } from "./v1";
```

The declaration describes JSON values. JSON Schema remains authoritative for
constraints such as numeric bounds and name patterns; the types do not perform
runtime validation.

A host that vendors lash cannot see this directory; it reads the same `v1.d.ts`
bytes from `lash::workflow::view::TYPESCRIPT_DECLARATION`, which `include_str!`s
the generated copy committed as
`crates/lash-kernel-edit/kernel-workflow-view-v1.d.ts`. The crate's laws check
both files hold the same bytes.

Both files are produced by `scripts/generate-workflow-schemas.py`, and
`kiln build //:schema_checks` checks both against fresh generation. To refresh:

```sh
. ./env.sh
kiln build //:host_schema_documents --materializations final --build-report .kiln/schema-build.json
python3 tools/buck2/outputs.py --report .kiln/schema-build.json --label //:host_schema_documents --single
python3 scripts/generate-workflow-schemas.py --generated <the-output-directory>
```
