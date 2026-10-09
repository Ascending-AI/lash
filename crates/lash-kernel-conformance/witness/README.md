Run captures from the repository root:

```sh
python3 crates/lash-kernel-conformance/witness/capture.py python \
  crates/lash-kernel-conformance/witness/fan-out.py \
  crates/lash-kernel-conformance/witness/fan-out.tools.json \
  crates/lash-kernel-conformance/witness/fan-out.python.json
python3 crates/lash-kernel-conformance/witness/capture.py javascript \
  crates/lash-kernel-conformance/witness/fan-out.mjs \
  crates/lash-kernel-conformance/witness/fan-out.tools.json \
  crates/lash-kernel-conformance/witness/fan-out.javascript.json
```

Python source exports `async def main(tool, output)`. JavaScript source exports
`async function main({tool, print})`. Programs are trusted witness inputs,
executed directly by system CPython or the vendored Node; this is not a
workflow interpreter. `--node` may explicitly name the vendored executable.

The script table lists calls in issue order, matching name and arguments,
with either `value` or `error: {kind, message, data}`. Delivery batches list
call numbers; every batch is delivered in order before coroutines run again.
Missing, repeated, mismatched or unconsumed rows fail capture. Both witnesses
print 1 and 2 before either answer, print 20 before 10, and return results in
member order. Checked-in captures record typed data, errors and issue/delivery
traces. JavaScript numbers are floats and Python integers are integers.

Capture is a maintainer operation. Conformance tests consume checked-in
results and do not invoke these runtimes or require a network.
