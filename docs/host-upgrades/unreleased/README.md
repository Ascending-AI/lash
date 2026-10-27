# Unreleased host changes

A change to a host-facing signature, wire shape, default or dependency floor
adds one file here named `<ticket>.md`, for example `FIG-7500.md`. Keep the entry
in the same commit as the change. Use one file per change so concurrent work
does not contend on a shared changelog. Internal changes need no entry.

Use this format, replacing every placeholder:

```markdown
# FIG-1234: Name the host change

| Old contract | New contract / host action | Commit and ticket | Missed-change diagnostic or symptom |
| --- | --- | --- | --- |
| Exact old signature, wire field or default | Exact replacement and required opt-ins, or state that none are needed | This commit, FIG-1234 | Compiler error, decoder error, startup refusal or observable behavior |

Source: [owning implementation](../../../crates/lash/src/artifacts.rs).
```

Use the actual ticket as the filename and title. Include every removed, renamed
or retyped public item and every configuration/feature/policy choice a host
needs. Give an old/new payload example when a field rename alone does not show
the change. Verify the entry against code. Link the owning implementation using
a path relative to the entry file. `This commit` avoids a self-referential SHA;
the release editor resolves it from the entry's introducing commit.

At a release cut, the release owner folds all ticket files into
[the upgrade guide](../../host-upgrades.md), grouped under the previous and new
release pins. Replace `This commit` with the introducing commit's SHA/link,
preserve ticket identities and diagnostics, adjust relative links, and remove
the folded ticket files in that release commit. Leave this README for the next
release. Do not drop an entry until its row is in the guide.

The [proof rules](../../agents/way-of-working.md#host-upgrade-entries) and
[AGENTS.md](../../../AGENTS.md) require these entries. Facade completeness checks
reachability and schema checks generated-file agreement; neither supplies a
before/after host-contract diff. They cannot precisely enforce this rule for
signatures, defaults and dependencies, so review checks it at landing.
