# Host contract schemas

This directory contains the checked-in JSON Schema documents for Lash's
host-facing contracts. Each shape has its own directory and one current file
named `v<version>.schema.json`. The document records the Rust version constant
in `x-lash-version-constant` and its numeric value in
`x-lash-schema-version`.

Run `python3 scripts/generate-workflow-schemas.py` after a schema owner changes.
Run the same command with `--check` to detect drift. The check also rejects an
obsolete versioned document left beside the current one.

The generator registry currently contains the landed graph-cutover shapes:

- `workflow-graph`, owned by `WORKFLOW_GRAPH_SCHEMA_VERSION`;
- `workflow-type-facets`, owned by `WORKFLOW_TYPE_FACET_SCHEMA_VERSION`.

Trace, remote protocol, and durable event owners add another registry entry and
a sibling shape directory when their cutovers land. They do not change the
layout or reuse another shape's version.
