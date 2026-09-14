/// The shared prelude every benchmark program carries.
///
/// ADR 0096 makes TypeScript the sole authored RLM dialect, so the benchmark
/// corpus is authored in TypeScript and lowered through `lash_typescript`.
/// `range` is a prelude helper rather than a builtin because the TypeScript
/// front-end reaches the VM through its own lowering, not through the IR's
/// builtin library.
const BENCH_PRELUDE: &str = r#"
function range(start: number, stop: number, step: number) {
  const out = [];
  let cursor = start;
  while (step > 0 ? cursor < stop : cursor > stop) {
    out.push(cursor);
    cursor = cursor + step;
  }
  return out;
}

const echo = defineProcess({
  name: "echo",
  signals: {},
  run: async (value: unknown) => { return value; }
});

const spawn_child = defineProcess({
  name: "spawn_child",
  signals: {},
  run: async (task: string, capability: string) => {
    return { claim: "done:" + task, capability: capability };
  }
});

const query_llm = defineProcess({
  name: "query_llm",
  signals: {},
  run: async (prompt: string, model: string) => {
    return { text: "benchmark summary", tokens: 42, model: model, prompt: prompt };
  }
});
"#;

pub fn benchmark_program(scenario: Scenario) -> String {
    let main = match scenario {
        Scenario::Baseline => {
            r#"
const items = [
  { label: "alpha", weight: 1, active: true },
  { label: "beta", weight: 2, active: false },
  { label: "gamma", weight: 3, active: true }
];
const indexes = range(0, items.length, 1);
const all_indexes = indexes.concat([items.length]);
let total = 0;
let labels = [];
for (const item of items) {
  total = total + item.weight;
  if (item.active) {
    labels = labels.concat([item.label + ":" + item.weight]);
  }
}
const lookup_handle = start(echo, { value: labels.join(",") });
const stats_handle = start(echo, {
  value: {
    total: total,
    count: items.length,
    seen: history.length,
    index_count: all_indexes.length
  }
});
const fanout = await Promise.all([lookup_handle, stats_handle]);
const lookup_value = fanout[0];
const stats_value = fanout[1];
finish(
  "user=" + ctx.user +
  ";attempt=" + ctx.attempt +
  ";active=" + lookup_value +
  ";total=" + stats_value.total +
  ";count=" + stats_value.count +
  ";seen=" + stats_value.seen +
  ";indexes=" + stats_value.index_count
);
"#
        }
        Scenario::LanguageHostEnvironment => {
            r#"
const source = history.join(",");
const tokens = source.split(",");
const trimmed_user = (" " + ctx.user + " ").trim();
const beta_index = source.indexOf("beta");
const line_matches = tokens.filter((line: string) => line.includes("a"));
const count = tokens.length;
const empty_tail = tokens.slice(count, count).length === 0;
const predicates = [
  source.includes(tokens[1]),
  source.startsWith(tokens[0]),
  source.endsWith(tokens[2])
];
const numeric = {
  neg: -ctx.attempt,
  sum: ctx.attempt + count,
  diff: count - 1,
  product: count * 2,
  quotient: count / 2,
  modulo: count % 2,
  parsed_int: Number.parseInt("" + ctx.attempt),
  parsed_float: Number.parseFloat("" + ctx.attempt)
};
const logic = !false && (count > 2 || empty_tail);
const comparisons = [
  count === 3,
  count !== 4,
  count < 4,
  count <= 3,
  count > 2,
  count >= 3
];
const choice = logic ? "yes" : "no";
const json_text = '{"attempt":' + ctx.attempt + ',"ok":true}';
const parsed = JSON.parse(json_text);
const positional = await Promise.all([
  start(echo, { value: "left:" + tokens[0] }),
  start(echo, { value: tokens[1] }),
  start(echo, { value: tokens.length })
]);
const named_results = await Promise.all([
  start(echo, { value: source }),
  start(echo, { value: trimmed_user + ":" + count })
]);
const named = { lookup: named_results[0], summary: named_results[1] };
const awaited = await start(echo, { value: "awaited" });
const direct = await tools.echo({ value: trimmed_user });
print(direct);
const state = {
  tags: tokens.concat(["delta"]),
  counts: {},
  kept: [],
  predicates: predicates,
  comparisons: comparisons,
  numeric: numeric,
  parsed: parsed,
  line_matches: line_matches
};
state.tags[1] = "beta";
const counts = {};
for (const token of state.tags) {
  counts[token] = (counts[token] === undefined ? 0 : counts[token]) + 1;
}
state.counts = counts;
for (const token of Object.keys(state.counts)) {
  if (token === "beta") {
    continue;
  }
  if (token === "delta") {
    break;
  }
  state.kept = state.kept.concat([token]);
}
finish({
  direct: direct,
  awaited: awaited,
  choice: choice,
  positional: positional,
  lookup: named.lookup,
  summary: named.summary,
  values: Object.values(state.counts),
  beta_index: beta_index,
  line_matches: line_matches,
  first_two: state.tags.slice(0, 2),
  kept: state.kept
});
"#
        }
        Scenario::AsyncAwait => {
            r#"
const results = await Promise.all([
  start(echo, { value: "alpha" }),
  start(echo, { value: "beta" }),
  start(echo, { value: "gamma" })
]);
finish([results[0], results[1], results[2]].join(","));
"#
        }
        Scenario::DirectUnwrap => {
            r#"
const first = await tools.echo({ value: "alpha" });
const second = await tools.echo({ value: first + ":" + "beta" });
const third = await tools.echo({ value: [first, second].join(",") });
finish(third);
"#
        }
        Scenario::GeneralFanout => {
            r#"
const seed = ["alpha", "beta", "gamma"];
const results = await Promise.all([
  start(echo, { value: seed[0] + ":" + seed.length }),
  start(echo, { value: seed[1] + ":" + seed.length })
]);
finish(results[0] + "|" + results[1]);
"#
        }
        Scenario::LoopControl => {
            r#"
const items = range(0, 128, 1);
// `outer` is deliberately shadowed by the loop binding: the scenario pins
// that the binding stays inside the loop and the outer one is untouched.
const outer = "restored";
let kept = 0;
let skipped = 0;
for (const outer of items) {
  if (outer < 32) {
    skipped = skipped + 1;
    continue;
  }
  if (outer >= 96) {
    break;
  }
  if (outer % 3 === 0) {
    continue;
  }
  kept = kept + 1;
}
finish({ kept: kept, skipped: skipped, outer: outer });
"#
        }
        Scenario::IndexedAssignment => {
            r#"
const groups = ["alpha", "beta", "alpha", "gamma", "beta", "alpha", "delta", "gamma"];
const counts = {};
for (const group of groups) {
  counts[group] = (counts[group] === undefined ? 0 : counts[group]) + 1;
}
const state = {
  groups: {
    alpha: { count: 0 },
    beta: { count: 0 },
    gamma: { count: 0 },
    delta: { count: 0 }
  }
};
for (const group of Object.keys(counts)) {
  state.groups[group].count = counts[group];
}
let summary = [];
for (const group of Object.keys(counts)) {
  summary = summary.concat([group + ":" + state.groups[group].count]);
}
finish({ counts: counts, state: state, summary: summary.join(",") });
"#
        }
        Scenario::ProjectedValues => {
            r#"
const first = history[0];
const second = history[1];
const body_head = docs.body[0];
const body_match_index = docs.body.indexOf("markdown");
const body_matches = docs.body.split("\n").filter((line: string) => line.includes("markdown"));
const second_matches = second.content.split("\n").filter((line: string) => line.includes("response"));
let body_truthy = false;
if (docs.body) {
  body_truthy = true;
}
print(docs.body);
finish({
  history_len: history.slice(0).length,
  first_role: first.role,
  first_content: first.content,
  second_content: second.content,
  doc_title: docs.title,
  doc_summary: docs.summary,
  body_head: body_head,
  body_match_index: body_match_index,
  body_matches: body_matches,
  second_matches: second_matches,
  body_truthy: body_truthy,
  body_text: docs.body
});
"#
        }
        Scenario::LargeData => {
            r#"
const items = range(0, 512, 1);
const groups = {};
let total = 0;
let evens = [];
let odds = [];
for (const item of items) {
  const key = "bucket_" + (item % 16);
  groups[key] = (groups[key] === undefined ? 0 : groups[key]) + 1;
  total = total + item;
  if (item % 2 === 0) {
    evens = evens.concat([item]);
  } else {
    odds = odds.concat([item]);
  }
}
let lines = [];
for (const key of Object.keys(groups)) {
  lines = lines.concat([key + ":" + groups[key]]);
}
finish({
  count: items.length,
  total: total,
  groups: groups,
  evens: evens.length,
  odds: odds.length,
  summary: lines.join("|")
});
"#
        }
        Scenario::CachePressure => {
            r#"
const seed = {
  user: ctx.user,
  attempt: ctx.attempt,
  history_len: history.length,
  labels: ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"]
};
const a0 = seed.labels[0] + ":" + seed.attempt;
const a1 = seed.labels[1] + ":" + seed.history_len;
const a2 = seed.labels[2] + ":" + a0.length;
const a3 = seed.labels[3] + ":" + a1.length;
const a4 = seed.labels[4] + ":" + a2.length;
const a5 = seed.labels[5] + ":" + a3.length;
const records = [
  { name: "r0", value: a0, next: a1 },
  { name: "r1", value: a1, next: a2 },
  { name: "r2", value: a2, next: a3 },
  { name: "r3", value: a3, next: a4 },
  { name: "r4", value: a4, next: a5 },
  { name: "r5", value: a5, next: a0 }
];
finish(records.map((record: unknown) => record.value).join("|"));
"#
        }
        Scenario::ProjectedOperations => {
            // `len` and `empty` read the projected list directly; TypeScript
            // spells that as `.length`, which lowers to a plain field read a
            // projection does not serve (FIG-3058), so both stay AST-built in
            // `ast_only_operations` and the projected path keeps being measured.
            r#"
finish({
  keys: Object.keys(proj.record),
  values: Object.values(proj.record),
  contains: proj.items.includes("beta"),
  starts: proj.text.startsWith("alpha"),
  ends: proj.text.endsWith("delta"),
  split_count: proj.text.split(" ").length,
  join: proj.items.join(","),
  trim: proj.padded.trim(),
  slice_text: proj.text.slice(6, 10),
  slice_list: proj.items.slice(1, 3),
  pushed: proj.items.concat(["epsilon"]),
  as_int: Number.parseInt("" + proj.number),
  as_float: Number.parseFloat("" + proj.number),
  parsed: JSON.parse(proj.json),
  first: proj.items[0],
  field: proj.record.topic
});
"#
        }
        Scenario::TypeSystemStress => {
            // The type language has no TypeScript spelling: ADR 0096 keeps
            // `Type { .. }` and `validate` in the IR, and the JSON-schema law
            // they pin lives in the property suite. What remains here is the
            // record-shaping cost this scenario was always measuring.
            r#"
let items = [];
for (const i of range(0, 64, 1)) {
  items = items.concat([{
    id: i,
    title: "item-" + i,
    score: i / 2,
    active: i % 2 === 0,
    meta: {
      source: ctx.user,
      attempt: ctx.attempt,
      tags: ["alpha", "beta", "gamma"]
    },
    maybe: i % 3 === 0 ? null : "v" + i
  }]);
}
finish({
  count: items.length,
  first: items[0].title,
  last: items[63].title,
  tags: items[1].meta.tags.join(",")
});
"#
        }
        Scenario::WrappedErrorPaths => {
            // A TypeScript tool call throws rather than returning an `{ ok,
            // error }` envelope, so the failure paths this scenario measures
            // are pinned through `try`/`catch` instead.
            r#"
let missing_error = "";
try {
  await tools.missing_tool({ value: "x" });
} catch (error) {
  missing_error = error.message;
}
let boom_error = "";
try {
  await tools.boom({ reason: "explicit" });
} catch (error) {
  boom_error = error.message;
}
const ok = await tools.echo({ value: "still-running" });
const probe = await shell.exec({ cmd: "test -f Cargo.lock" });
finish({
  missing_error: missing_error.includes("unknown tool"),
  boom_error: boom_error.includes("explicit failure"),
  ok_value: ok,
  probe_exit: probe.exit_code,
  probe_done: probe.done
});
"#
        }
        Scenario::ToolControlHostEnvironment => {
            r#"
const first = start(spawn_child, { task: "inspect auth", capability: "explore" });
const second = start(spawn_child, { task: "inspect api", capability: "explore" });
const llm = start(query_llm, { prompt: "summarize benchmark", model: "gpt-5.4-mini" });
const probe = start(echo, { value: "app log" });
const handles = await processes.list({});
const results = await Promise.all([first, second, llm, probe]);
finish({
  first: results[0].claim,
  second: results[1].claim,
  llm: results[2].text,
  probe: results[3],
  tools: handles.length
});
"#
        }
        Scenario::SnapshotProjectedState => {
            r#"
const head = snap.projected.body.slice(0, 16);
const materialized = "" + snap.projected.body;
const nested_head = snap.mixed.nested.projected_title;
snap.mixed.count = snap.mixed.count + 1;
finish({
  id: snap.id,
  normal_title: snap.normal.title,
  head: head,
  materialized_len: materialized.length,
  nested_head: nested_head,
  count: snap.mixed.count,
  tags: snap.normal.tags.join(",")
});
"#
        }
        Scenario::ContinueAsSeedHostEnvironment => {
            r#"
const agent = start(spawn_child, { task: "inspect carry-forward", capability: "explore" });
const handles = await processes.list({});
const frame = await control.continue_as({
  task: "continue from compact state",
  seed: {
    projected_problem: proj.text,
    nested_projected: { body: proj.json },
    computed_summary: ctx.user + ":" + history.length,
    live_agent: handles[0],
    started_agent: agent
  }
});
finish({
  frame_key: frame.frame_key,
  task: frame.task,
  seed_keys: frame.seed_keys,
  projected_count: frame.projected_count,
  global_count: frame.global_count
});
"#
        }
        Scenario::TriggerRegistryHostEnvironment => {
            r#"
const daily_digest = defineProcess({
  name: "daily_digest",
  signals: {},
  run: async (tick: unknown) => { return { kind: "daily_digest", fired_at: tick.fired_at }; }
});
const on_button = defineProcess({
  name: "on_button",
  signals: {},
  run: async (event: unknown) => { return { kind: "button", button: event.button }; }
});
const daily_handle = await registerTrigger({
  source: cron.Schedule({ expr: "0 8 * * *", tz: "UTC" }),
  target: daily_digest,
  inputs: (event) => ({ tick: event }),
  name: "daily_digest",
  subscription_key: "daily-digest"
});
const button_handle = await registerTrigger({
  source: ui.button.pressed({}),
  target: on_button,
  inputs: (event) => ({ event: event }),
  name: "button watcher",
  subscription_key: "button-watcher"
});
const registrations = await triggers.list({ target: daily_digest });
const disabled = await triggers.disable({
  subscription_key: "daily-digest",
  expected_revision: daily_handle.revision
});
finish({
  daily_handle: daily_handle.id,
  button_handle: button_handle.id,
  registration_count: registrations.length,
  listed_target: registrations[0].target.process_name,
  listed_source: registrations[0].source_type,
  disabled: disabled.enabled
});
"#
        }
        Scenario::SyntaxTextHostEnvironment => {
            // Parser-heavy text forms, re-pinned to the shapes the TypeScript
            // front-end actually has to lex: template literals with
            // substitutions and raw braces, escaped quotes, comments, and
            // multi-line string content.
            r#"
// Exercise lexer-heavy string forms, comments, and text methods.
const marker = "Patch";
const patch = `*** Begin ${marker}
*** Update File: crates/lashlang/src/lib.rs
@@
-old
+new
\\n { braces stay raw }
*** End ${marker}`;
const script = `python3 - <<'PY'
print("""double quotes are preserved""")
\\n { braces stay raw }
PY`;
const plain = `first
"quoted"
second`;
const pieces = [
  patch.length,
  patch.includes("*** Begin Patch"),
  script.startsWith("python3"),
  script.trim().endsWith("PY"),
  plain.split("\n").length,
  plain.slice(0, 5)
];
finish({
  patch_head: patch.slice(0, 15),
  script_head: script.slice(0, 7),
  plain_lines: plain.split("\n").length,
  pieces: pieces
});
"#
        }
        Scenario::IntegerRangeHostEnvironment => {
            r#"
const items = range(-8, 9, 1);
const forward = range(0, 10, 3);
const backward = range(7, -3, -2);
const stride = Math.ceil(items.length / 4);
let starts = [];
let windows = [];
for (const i of range(0, items.length, stride)) {
  starts = starts.concat([i]);
  windows = windows.concat([items.slice(i, i + stride)]);
}
const text = "alpha beta gamma beta delta";
const first_beta = text.indexOf("beta");
const second_beta = text.indexOf("beta", first_beta + 1);
finish({
  count: items.length,
  first: items[0],
  last: items[items.length - 1],
  forward: forward,
  backward: backward,
  stride: stride,
  starts: starts,
  windows: windows,
  mid: items.slice(2, -2),
  head: text.slice(0, 5),
  tail: text.slice(-5),
  first_beta: first_beta,
  second_beta: second_beta,
  ceil_neg: Math.ceil(-10 / 3),
  floor_neg: Math.floor(-10 / 3)
});
"#
        }
        Scenario::FanoutExpressionHostEnvironment => {
            r#"
const left = await tools.echo({ value: "left" });
const right = await tools.echo({ value: "right" });
const computed = history.length + 39;
const discarded = ["branch_a", 40 + 2, history.length];
const batched_results = await Promise.all([
  start(echo, { value: left }),
  start(echo, { value: right })
]);
const batched = {
  first: batched_results[0],
  second: batched_results[1],
  computed: computed
};
finish({
  left: left,
  right: right,
  computed: computed,
  discarded: discarded,
  first: batched.first,
  second: batched.second,
  batched_computed: batched.computed
});
"#
        }
        Scenario::ImageHostEnvironment => {
            r#"
const descriptor = JSON.stringify(img);
const metadata = {
  id: img.id,
  label: img.label,
  size: img.size,
  width: img.width,
  height: img.height,
  missing: img.missing
};
print(img);
finish({
  metadata: metadata,
  descriptor_has_type: descriptor.includes("\"type\":\"image\""),
  descriptor_has_id: descriptor.includes("\"id\":\"img-1\""),
  dims: img.width + "x" + img.height,
  size_bucket: Math.floor(img.size / 100)
});
"#
        }
        // Heap-shaped scenarios. These exist because a wall-clock cliff in
        // iteration, allocation churn or descendant mutation was invisible to
        // every other scenario here: none of them iterate a long list, and
        // allocation-count budgets say nothing about time.
        Scenario::HeapListIteration => {
            // One pass over a long heap-backed list. Per-step work must not
            // depend on the length of the list being iterated.
            r#"
let rows = [];
for (const n of range(0, 2000, 1)) {
  rows = rows.concat([n]);
}
let total = 0;
let seen = 0;
for (const row of rows) {
  total = total + row;
  seen = seen + 1;
}
finish({ total: total, seen: seen });
"#
        }
        Scenario::HeapNestedLoop => {
            // An inner pass over a list that the outer loop keeps growing.
            r#"
let rows = [];
let checksum = 0;
for (const n of range(0, 60, 1)) {
  rows = rows.concat([[n, n + 1]]);
  for (const row of rows) {
    checksum = checksum + row[0];
  }
}
finish({ checksum: checksum, rows: rows.length });
"#
        }
        Scenario::HeapAllocationChurn => {
            // Short-lived containers built and dropped in a loop, with a small
            // retained set surviving across collections.
            r#"
let kept = [];
for (const n of range(0, 400, 1)) {
  const scratch = { index: n, pair: [n, n + 1], label: "row-" + n };
  if (n % 40 === 0) {
    kept = kept.concat([scratch]);
  }
}
finish({ kept: kept.length });
"#
        }
        Scenario::HeapDeepChainMutation => {
            // Repeated writes to a descendant reached through a path, which is
            // the shape that walks reverse parent edges.
            r#"
const tree = { level: { rows: [[0], [1], [2]], counters: { c: 0 } } };
for (const n of range(0, 150, 1)) {
  tree.level.rows[n % 3] = [n];
  tree.level.counters["c"] = tree.level.counters["c"] + 1;
}
finish({ counter: tree.level.counters["c"], first: tree.level.rows[0] });
"#
        }
        Scenario::HeapVariableConcat => {
            // `acc = acc.concat(other)` where the right operand is a bare
            // variable. Every other scenario concatenates a fresh single-item
            // literal, so this is the only guard on the general concat's
            // per-iteration cost.
            r#"
const other = [1, 2];
let acc = [];
for (const n of range(0, 300, 1)) {
  acc = acc.concat(other);
}
finish({ total: acc.length, head: acc[0] });
"#
        }
        Scenario::HeapShallowChainMutation => {
            // The shallow half of the depth pair: same write count, six levels
            // of nesting.
            r#"
let tree = { leaf: [0] };
for (const level of range(0, 6, 1)) {
  tree = { next: tree };
}
let deep = tree;
for (const level of range(0, 6, 1)) {
  deep = deep.next;
}
for (const n of range(0, 150, 1)) {
  deep.leaf = [n];
}
finish({ leaf: deep.leaf });
"#
        }
        Scenario::HeapDeepChainMutation24 => {
            // The deep half: the identical program at twenty-four levels, so
            // the two scenarios differ only in ancestor depth and their ratio
            // is the scaling guard. Twenty-four is four times the shallow half,
            // which is all the ratio reads; the depth is not a stack ceiling in
            // disguise, since linking is now bounded per level well inside the
            // budget (see `lower_expr_expected_inner`).
            r#"
// Both halves of the pair build their tree with a loop and reach the leaf
// through a walked reference, so the two programs are byte-for-byte the same
// shape apart from the depth constant. A twenty-four-deep object literal (or
// member chain) is past the front-end's source nesting limit, and what the
// ratio reads is the heap ancestor depth of the mutated node, not the length
// of the path spelled at the write site.
let tree = { leaf: [0] };
for (const level of range(0, 24, 1)) {
  tree = { next: tree };
}
let deep = tree;
for (const level of range(0, 24, 1)) {
  deep = deep.next;
}
for (const n of range(0, 150, 1)) {
  deep.leaf = [n];
}
finish({ leaf: deep.leaf });
"#
        }
        Scenario::HeapComprehensionBuild => {
            // A mapped build over a long source list. Appending to the
            // accumulator must not rebuild it once per element.
            r#"
let source = [];
for (const n of range(0, 800, 1)) {
  source = source.concat([n]);
}
const doubled = source.map((item: number) => item + item);
const tagged = source.filter((item: number) => item % 7 === 0).map((item: number) => [item]);
finish({ doubled: doubled.length, tagged: tagged.length, last: doubled[doubled.length - 1] });
"#
        }
    };
    format!("{BENCH_PRELUDE}\n{main}")
}

/// Restores the measured operations the TypeScript dialect has no syntax for.
///
/// ADR 0096 keeps `cancel`, `validate`/`Type {}` and the projected `len`/`empty`
/// reads in the IR while the authored surface can no longer spell them, so the
/// corpus lowers what TypeScript expresses and builds the rest from the public
/// AST constructors. Without this the guard would silently stop measuring
/// `AbilityOp::Cancel`, the validator, and the projected-read path, and the
/// max-only budgets would pass on a smaller program.
///
/// Statements are spliced in ahead of the trailing `finish` and result fields
/// are appended to the record it returns, so the scenario keeps its shape. The
/// span vectors are positional and the appended nodes have no source text, so
/// they are dropped rather than left misaligned.
pub fn with_ast_only_operations(scenario: Scenario, mut program: Program) -> Program {
    let (statements, fields) = ast_only_operations(scenario);
    if statements.is_empty() && fields.is_empty() {
        return program;
    }

    let Expr::Block(mut block) = program.main else {
        panic!("benchmark program body should be a block");
    };
    let Some(Expr::Finish(value)) = block.pop() else {
        panic!("benchmark program should end in finish");
    };
    block.extend(statements);
    let value = if fields.is_empty() {
        *value
    } else {
        let Expr::Record(mut entries) = *value else {
            panic!("benchmark scenario should finish a record");
        };
        entries.extend(fields);
        Expr::Record(entries)
    };
    block.push(Expr::Finish(Box::new(value)));
    program.main = Expr::Block(block);
    program.expression_spans.clear();
    program.expression_source_spans.clear();
    program
}

type AstOnlyOperations = (Vec<Expr>, Vec<(compact_str::CompactString, Expr)>);

fn ast_only_operations(scenario: Scenario) -> AstOnlyOperations {
    match scenario {
        Scenario::LanguageHostEnvironment => (
            vec![
                ast_assign(
                    "cancelled",
                    Expr::StartProcess(ProcessStartExpr {
                        process: "echo".into(),
                        args: vec![("value".into(), Expr::String("cancelled".into()))],
                    }),
                ),
                Expr::Cancel(Box::new(ast_variable("cancelled"))),
                ast_assign(
                    "validated",
                    ast_builtin(
                        "validate",
                        vec![
                            Expr::Record(vec![
                                ("user".into(), ast_variable("direct")),
                                ("choice".into(), ast_variable("choice")),
                                ("tags".into(), ast_field(ast_variable("state"), "tags")),
                                ("counts".into(), ast_field(ast_variable("state"), "counts")),
                                ("kept".into(), ast_field(ast_variable("state"), "kept")),
                                ("maybe".into(), Expr::Null),
                            ]),
                            Expr::TypeLiteral(Box::new(payload_type())),
                        ],
                    ),
                ),
            ],
            vec![
                ("validated".into(), ast_variable("validated")),
                (
                    "stringified".into(),
                    ast_builtin("to_string", vec![ast_variable("validated")]),
                ),
            ],
        ),
        Scenario::ToolControlHostEnvironment => (
            vec![Expr::Cancel(Box::new(ast_variable("second")))],
            Vec::new(),
        ),
        Scenario::ProjectedOperations => (
            Vec::new(),
            vec![
                (
                    "len".into(),
                    ast_builtin("len", vec![ast_field(ast_variable("proj"), "items")]),
                ),
                (
                    "empty".into(),
                    ast_builtin("empty", vec![ast_field(ast_variable("proj"), "items")]),
                ),
            ],
        ),
        _ => (Vec::new(), Vec::new()),
    }
}

/// The validator shape `language_host_environment` measures.
fn payload_type() -> TypeExpr {
    TypeExpr::Object(vec![
        TypeField {
            name: "user".into(),
            ty: TypeExpr::Str,
            optional: false,
        },
        TypeField {
            name: "choice".into(),
            ty: TypeExpr::Enum(vec!["yes".into(), "no".into()]),
            optional: false,
        },
        TypeField {
            name: "tags".into(),
            ty: TypeExpr::List(Box::new(TypeExpr::Str)),
            optional: false,
        },
        TypeField {
            name: "counts".into(),
            ty: TypeExpr::Dict,
            optional: false,
        },
        TypeField {
            name: "kept".into(),
            ty: TypeExpr::List(Box::new(TypeExpr::Str)),
            optional: false,
        },
        TypeField {
            name: "maybe".into(),
            ty: TypeExpr::Union(vec![TypeExpr::Str, TypeExpr::Null]),
            optional: false,
        },
        TypeField {
            name: "optional_note".into(),
            ty: TypeExpr::Str,
            optional: true,
        },
    ])
}
