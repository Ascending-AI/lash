The FIG-6352 / Design 8 corpus is the source catalog for workflow examples
and the baseline for studying lowering readability. `catalog.json` names
both dialects' inputs, entry/refusal contracts and source-map exceptions.
Each dialect pins its own values and signatures. Only entry names and shared
refusal codes are compared between dialects.

From the repository root, after sourcing `env.sh`, bless all readable outputs
with one command:

```sh
kiln run //examples/workflows:workflow-corpus__bin -- --bless "$PWD/examples/workflows"
```

Review the resulting diffs. Blessing checks reproducible lowering, canonical
print/read equality, SM-ORDER, SM-COVER, declaration-only view derivation,
expanded Transaction replay and real VM function crossing before writing.
The corpus law compares all generated outputs with the committed files:

```sh
kiln test //examples/workflows:corpus__test --test_arg=design_8_corpus_pins_readable_per_dialect_outputs --test_arg=--exact
```

`kernel.<lang>.txt` is kernel source plus per-entry statement, scope and site
counts. `source-map.<lang>.txt` joins mapped sites to byte ranges and source
text and names every unmapped synthetic site. `view.<lang>.txt` gives statements,
scopes and callees with ids, names and labels, derived from shipped declarations.
`printed.<lang>` is exact printer output. `edits/1.<lang>.json` records an
expanded Transaction, its base and effect context; its `.txt` shows the kernel
diff, Correspondence and reprinted source. Refused cases pin stable codes.

Cell inputs are `source.<lang>`; `cell-1.<lang>.txt` shows the crossing datum
structure and the FunctionValue's normalized code and frozen captures. Helper
references use names here, so incidental content ids do not obscure the pin.
The capturing cell changes a shared list after shipping the function, then
starts the frozen document with `x=2`. Each dialect pins its own result.
FN-ID independently proves content identity stability.

`laws.json` keeps every named source-map/workflow/crossing/start law directly
runnable, including its existing owning test. Run selected rules once by full
path with:

```sh
python3 examples/workflows/laws.py --rule WR-PARK --rule FN-RESUME
```

With no `--rule`, it runs all named test laws. `--rule FEATURE-CUT` invokes the
existing feature witness through `kiln gate`. The runner groups selectors by
target, refuses a zero-test receipt, records exact commands and receipts under
`.kiln/FIG-6384/`, and never blesses a failure. It uses SQLite for durable laws.
