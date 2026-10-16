The page and carry wire is checked under `kiln build //:schema_checks`.
`worker_wire_schema_generator` derives the reachable schemas from `StartWire`,
page requests and replies (with their exchange ids), and `SealBatch`. The schemas
in `schemas/host/worker-*` are the reviewed 1.0 baseline. These roots do not have
feature-dependent variants, so the same schema applies with `testing` enabled.

`PageCapability` uses a custom encoding: 81 packed bytes in MessagePack and hex
in JSON. Its schema describes the JSON string and retains its type-derived
fields under `x-lash-packed-fields`, with the binary layout alongside them.
The independent vectors in `tests/wire_schema.rs` pin the actual custom codec,
including authority tags, little-endian words, maximal values, and carry links.
`PageEnvelope` crosses as bytes; its derived digest does not cross.

To author a baseline update before the 1.0 cut, build
`kiln build //:host_schema_documents --materializations final`, then pass that
output directory to `python3 scripts/generate-workflow-schemas.py --generated
<directory>`. Review the changed schemas. Alias and type-path spelling are not
part of the serialized shape. Protocol versions stay at the 1.0 baseline.
