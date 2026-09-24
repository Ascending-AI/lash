//! The source programs the workflow-graph golden tests pin.
//!
//! They live here, not inline in their tests, because the corpus laws
//! (`tests/corpus_laws.rs`) run the print/reparse/admit round trip and the
//! artifact invariants over every corpus the dialect has, and the goldens are
//! one of them (FIG-3599). One constant, read by both, keeps the two from
//! drifting apart.

#![allow(
    dead_code,
    reason = "each including binary reads the constants it pins"
)]

/// The lens-law fixture: a lifted process with a loop and a signal wait, a
/// collection pipeline, a branch and a finish.
pub(crate) const REPRESENTATIVE: &str = r#"const child = async (input: unknown) => {
    let total = 0;
    for (const value of input.values) {
      await sleep(1);
    }
    const signal = await waitSignal("refresh");
    return total;
  };
const items = [1, 2, 3].filter((value) => value > 1).map((value) => value * 2);
if (items.length > 0) {
  console.log(items);
} else {
  console.log("empty");
}
finish(items);
"#;

/// The IR JSON golden's program.
pub(crate) const IR_JSON: &str = "await tools.lookup({ query: \"x\" });\nawait sleep(\"1s\");\n";

/// The facet golden's program.
pub(crate) const WITH_FACETS: &str =
    "const answer = await tools.lookup({ query: \"x\" });\nfinish(answer);\n";

/// The canonical-span golden with named, nested and repeated statements.
pub(crate) const SPAN_NAMED_NESTED_REPEATED: &str = r#"const worker=async()=>{await tools.echo({value:"same"});await tools.echo({value:"same"});if(true){for(const value of [1]){while(false){await sleep(value);}}}return "done";};"#;

/// The canonical-span golden whose process literal lifts inline. Its source
/// is a registered trigger source constructor, so a host admits it.
pub(crate) const SPAN_LIFTED_INLINE: &str = r#"await triggers.register({source:timer.Schedule({expr:"0 8 * * *"}),target:async(event)=>{await tools.echo({value:"inline"});return event;}});"#;

/// FIG-3635's shape: top-level `var` declarations hoisted ahead of the
/// hoisted function declarations, so the round trip only holds when the
/// canonical print spells the hoist `var`.
pub(crate) const VAR_HOIST_FUNCTION: &str = r#"var x = function () {
  return 1;
};
var y = function () {
  return 2;
};
function f_arg() {}
f_arg();
finish(x);
"#;

/// The carrier laws' corpus (FIG-3571 L3/L4): sources that exercise every
/// structure the ownership walk distinguishes.
pub(crate) const CARRIER_LAWS: &[&str] = &[
    // A multi-statement loop body with a branch, a key loop, plain and
    // compound member assignment in braced and unbraced arms, try, and an
    // array callback.
    r#"const items = [1, 2];
const box = { value: 0 };
for (const item of items) {
  await tools.echo({ value: item });
  if (item > 1) {
    await tools.echo({ value: "then" });
  } else {
    box.value = await tools.echo({ value: "else" });
  }
}
for (const field in box) {
  await tools.echo({ value: field });
  await tools.echo({ value: "keys" });
}
if (box.value === 0) box.value = await tools.echo({ value: 1 });
if (box.value === 1) {
  box.value += 2;
}
try {
  await tools.echo({ value: "try" });
} catch (error) {
  await tools.echo({ value: "catch" });
}
const doubled = items.map((item) => item * 2);
finish(doubled);
"#,
    // A process literal with a multi-statement loop, a literal nested in it,
    // and an async closure (an arrow in a record field is a closure, not a
    // process literal).
    r#"const worker = async (limit: number) => {
  for (const step of [1, 2]) {
    await tools.echo({ value: step });
    await tools.echo({ value: limit });
  }
  const inner = async () => {
    await tools.echo({ value: "inner" });
    return 1;
  };
  const nested = await processes.start({ definition: inner });
  const handle = await processes.start({
    definition: async () => {
      await tools.echo({ value: "closure" });
      return 1;
    }
  });
  return 2;
};
const started = await processes.start({ definition: worker, args: { limit: 3 } });
finish("started");
"#,
    // The number literals whose identity the one IR number rule settles.
    "const values = [NaN, Infinity, -Infinity, 0, -0];\nfinish(values.length);\n",
    // Non-canonically formatted source.
    "const   a=1;for(const x of [a,2]){await tools.echo({value:x});await tools.echo({value:a})}\nfinish(a)",
];

/// Every golden program, by name.
pub(crate) const ALL: &[(&str, &str)] = &[
    ("representative", REPRESENTATIVE),
    ("ir-json", IR_JSON),
    ("with-facets", WITH_FACETS),
    ("span-named-nested-repeated", SPAN_NAMED_NESTED_REPEATED),
    ("span-lifted-inline", SPAN_LIFTED_INLINE),
    ("var-hoist-function", VAR_HOIST_FUNCTION),
];
