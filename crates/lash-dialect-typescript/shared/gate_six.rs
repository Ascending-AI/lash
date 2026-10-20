//! The programs of the kernel's speed gate (kernel workflow spec §11, gate
//! 6), the tools they call and the scripted answers a host gives them.
//! `examples/kernel_perf.rs` times them; the step-structure law runs them.

use std::collections::BTreeMap;

use lash_kernel_doc::{Datum, EffectName, ErrorDatum, Float, Name, Param, Signature, Type};
use lash_kernel_vm::Outcome;

/// The session data the old benchmark seeded or projected, as the literals a
/// cell would hold.
const CONTEXT: &str = r#"const history = ["alpha", "beta", "gamma"];
const ctx = { user: "sam", attempt: 3 };
"#;

const PROJECTED: &str = r#"const docs = { title: "Authoring Rules", summary: "Rules for authoring", body: "lazy markdown body" };
const proj = { items: ["alpha", "beta", "gamma", "delta"], text: "alpha beta gamma delta", padded: "  alpha  ", json: "{\"ok\":true,\"count\":4}", number: "4", record: { topic: "bench", count: 4 } };
"#;

pub(crate) fn programs() -> Vec<(&'static str, String)> {
    let mut programs: Vec<(&'static str, String)> = vec![
        ("baseline", format!("{CONTEXT}{}", r#"
const items = [{ label: "alpha", weight: 1, active: true }, { label: "beta", weight: 2, active: false }, { label: "gamma", weight: 3, active: true }];
const indexes: number[] = [];
for (let i = 0; i < items.length; i++) { indexes.push(i); }
indexes.push(items.length);
let total = 0;
const labels: string[] = [];
for (const item of items) {
  total = total + item.weight;
  if (item.active) { labels.push(`${item.label}:${item.weight}`); }
}
const lookup = perf.echo(labels.join(","));
const stats = perf.echo({ total: total, count: items.length, seen: history.length, index_count: indexes.length });
const fanout = await Promise.all([lookup, stats]);
const lookupValue = fanout[0];
const statsValue = fanout[1];
await perf.finish(`user=${ctx.user};attempt=${ctx.attempt};active=${lookupValue};total=${statsValue.total};count=${statsValue.count};seen=${statsValue.seen};indexes=${statsValue.index_count}`);
"#)),
        ("language_host_environment", format!("{CONTEXT}{}", r#"
const source = history.join(",");
const tokens = source.split(",");
const trimmedUser = ` ${ctx.user} `.trim();
const betaIndex = source.indexOf("beta");
const lineMatches = tokens.join("\n").split("\n").filter((line) => line.includes("a"));
const count = tokens.length;
const emptyTail = tokens.slice(count, count).length === 0;
const predicates = [source.includes(tokens[1]), source.startsWith(tokens[0]), source.endsWith(tokens[2])];
const numeric = { neg: -ctx.attempt, sum: ctx.attempt + count, diff: count - 1, product: count * 2, quotient: count / 2, modulo: count % 2, parsed_int: Math.trunc(ctx.attempt), parsed_float: Number(ctx.attempt) };
const logic = !false && (count > 2 || emptyTail);
const comparisons = [count === 3, count !== 4, count < 4, count <= 3, count > 2, count >= 3];
const choice = logic ? "yes" : "no";
const jsonText = "{\"attempt\":" + String(ctx.attempt) + ",\"ok\":true}";
const parsed = JSON.parse(jsonText);
const positionalResults = await Promise.all([perf.echo(`left:${tokens[0]}`), perf.echo(tokens[1]), perf.echo(tokens.length)]);
const positional = [positionalResults[0], positionalResults[1], positionalResults[2]];
const namedResults = await Promise.all([perf.echo(source), perf.echo(`${trimmedUser}:${count}`)]);
const named = { lookup: namedResults[0], summary: namedResults[1] };
const handle = perf.echo("awaited");
const awaited = await handle;
const direct = await perf.echo(trimmedUser);
console.log(direct);
tokens.push("delta");
const state = { tags: tokens, counts: {}, kept: [], predicates: predicates, comparisons: comparisons, numeric: numeric, parsed: parsed, line_matches: lineMatches };
state.tags[1] = "beta";
const counts = {};
for (const token of state.tags) { counts[token] = (counts[token] || 0) + 1; }
state.counts = counts;
for (const token of Object.keys(state.counts)) {
  if (token === "beta") { continue; }
  if (token === "delta") { break; }
  state.kept.push(token);
}
const validated = { user: direct, choice: choice, tags: state.tags, counts: state.counts, kept: state.kept, maybe: null };
await perf.finish({ direct: direct, awaited: awaited, positional: positional, lookup: named.lookup, summary: named.summary, values: Object.values(state.counts), beta_index: betaIndex, line_matches: lineMatches, first_two: state.tags.slice(0, 2), validated: validated, stringified: JSON.stringify(validated) });
"#)),
        ("async_await", r#"
const handles = [perf.echo("alpha"), perf.echo("beta"), perf.echo("gamma")];
const results = await Promise.all(handles);
await perf.finish([results[0], results[1], results[2]].join(","));
"#.to_string()),
        ("direct_unwrap", r#"
const first = await perf.echo("alpha");
const second = await perf.echo(`${first}:beta`);
const third = await perf.echo([first, second].join(","));
await perf.finish(third);
"#.to_string()),
        ("general_fanout", r#"
const seed = ["alpha", "beta", "gamma"];
const results = await Promise.all([perf.echo(`${seed[0]}:${seed.length}`), perf.echo(`${seed[1]}:${seed.length}`)]);
await perf.finish(`${results[0]}|${results[1]}`);
"#.to_string()),
        ("loop_control", r#"
let outer: number | string = "restored";
let kept = 0;
let skipped = 0;
for (let i = 0; i < 128; i++) {
  outer = i;
  if (i < 32) { skipped = skipped + 1; continue; }
  if (i >= 96) { break; }
  if (i % 3 === 0) { continue; }
  kept = kept + 1;
}
await perf.finish({ kept: kept, skipped: skipped, outer: outer });
"#.to_string()),
        ("indexed_assignment", r#"
const groups = ["alpha", "beta", "alpha", "gamma", "beta", "alpha", "delta", "gamma"];
const counts = {};
for (const group of groups) { counts[group] = (counts[group] || 0) + 1; }
const state = { groups: { alpha: { count: 0 }, beta: { count: 0 }, gamma: { count: 0 }, delta: { count: 0 } } };
for (const group of Object.keys(counts)) { state.groups[group].count = counts[group]; }
const summary: string[] = [];
for (const group of Object.keys(counts)) { summary.push(`${group}:${state.groups[group].count}`); }
await perf.finish({ counts: counts, state: state, summary: summary.join(",") });
"#.to_string()),
        ("projected_values", format!("{PROJECTED}{}", r#"
const history = [{ role: "user", content: "alpha request" }, { role: "assistant", content: "beta response" }, { role: "tool", content: "gamma observation" }];
const first = history[0];
const second = history[1];
const bodyHead = docs.body[0];
const bodyMatchIndex = docs.body.indexOf("markdown");
const bodyMatches = docs.body.split("\n").filter((line) => line.includes("markdown"));
const secondMatches = second.content.split("\n").filter((line) => line.includes("response"));
let bodyTruthy = false;
if (docs.body) { bodyTruthy = true; }
console.log(docs.body);
await perf.finish({ history_len: history.length, first_role: first.role, first_content: first.content, second_content: second.content, doc_title: docs.title, doc_summary: docs.summary, body_head: bodyHead, body_match_index: bodyMatchIndex, body_matches: bodyMatches, second_matches: secondMatches, body_truthy: bodyTruthy, body_text: docs.body });
"#)),
        ("large_data", r#"
const groups = {};
let total = 0;
const evens: number[] = [];
const odds: number[] = [];
for (let item = 0; item < 512; item++) {
  const key = `bucket_${item % 16}`;
  groups[key] = (groups[key] || 0) + 1;
  total = total + item;
  if (item % 2 === 0) { evens.push(item); } else { odds.push(item); }
}
const lines: string[] = [];
for (const key of Object.keys(groups)) { lines.push(`${key}:${groups[key]}`); }
await perf.finish({ count: 512, total: total, groups: groups, evens: evens.length, odds: odds.length, summary: lines.join("|") });
"#.to_string()),
        ("cache_pressure", format!("{CONTEXT}{}", r#"
const seed = { user: ctx.user, attempt: ctx.attempt, history_len: history.length, labels: ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"] };
const a0 = `${seed.labels[0]}:${seed.attempt}`;
const a1 = `${seed.labels[1]}:${seed.history_len}`;
const a2 = `${seed.labels[2]}:${a0.length}`;
const a3 = `${seed.labels[3]}:${a1.length}`;
const a4 = `${seed.labels[4]}:${a2.length}`;
const a5 = `${seed.labels[5]}:${a3.length}`;
const r0 = { name: "r0", value: a0, next: a1 };
const r1 = { name: "r1", value: a1, next: a2 };
const r2 = { name: "r2", value: a2, next: a3 };
const r3 = { name: "r3", value: a3, next: a4 };
const r4 = { name: "r4", value: a4, next: a5 };
const r5 = { name: "r5", value: a5, next: a0 };
const validated = [r0, r1, r2, r3, r4, r5];
await perf.finish([validated[0].value, validated[1].value, validated[2].value, validated[3].value, validated[4].value, validated[5].value].join("|"));
"#)),
        ("projected_operations", format!("{PROJECTED}{}", r#"
const pushed = proj.items.slice(0);
pushed.push("epsilon");
await perf.finish({ len: proj.items.length, empty: proj.items.length === 0, keys: Object.keys(proj.record), values: Object.values(proj.record), contains: proj.items.includes("beta"), starts: proj.text.startsWith("alpha"), ends: proj.text.endsWith("delta"), split_count: proj.text.split(" ").length, join: proj.items.join(","), trim: proj.padded.trim(), slice_text: proj.text.slice(6, 10), slice_list: proj.items.slice(1, 3), pushed: pushed, as_int: Math.trunc(Number(proj.number)), as_float: Number(proj.number), parsed: JSON.parse(proj.json), first: proj.items[0], field: proj.record.topic });
"#)),
        ("type_system_stress", format!("{CONTEXT}{}", r#"
type Meta = { source: string; attempt: number; tags: string[] };
type Item = { id: number; title: string; score: number; active: boolean; meta: Meta; maybe: string | null };
const items: Item[] = [];
for (let i = 0; i < 64; i++) {
  const raw: Item = { id: i, title: `item-${i}`, score: i / 2, active: i % 2 === 0, meta: { source: ctx.user, attempt: ctx.attempt, tags: ["alpha", "beta", "gamma"] }, maybe: i % 3 === 0 ? null : `v${i}` };
  items.push(raw);
}
await perf.finish({ count: items.length, first: items[0].title, last: items[63].title, tags: items[1].meta.tags.join(",") });
"#)),
        ("wrapped_error_paths", r#"
let missingOk = true;
let missingError = false;
try { await perf.boom("unknown tool missing_tool"); } catch (error) { missingOk = false; missingError = String(error.message).includes("unknown tool"); }
let boomOk = true;
let boomError = false;
try { await perf.boom("explicit failure"); } catch (error) { boomOk = false; boomError = String(error.message).includes("explicit failure"); }
const ok = await perf.echo("still-running");
const probe = await jobs.run({ target: "check Cargo.lock" });
await perf.finish({ missing_ok: missingOk, missing_error: missingError, boom_ok: boomOk, boom_error: boomError, ok_value: ok, probe_exit: probe.exit_code, probe_done: probe.done });
"#.to_string()),
        ("tool_control_host_environment", tool_control()),
        ("snapshot_projected_state", r#"
const snap = { id: "snapshot-mixed", normal: { title: "Snapshot Rules", tags: ["snapshot", "projected", "mixed"] }, projected: { body: "projected body stays lazy across snapshot markers" }, mixed: { count: 7, nested: { projected_title: "Nested Projection" } } };
const head = snap.projected.body.slice(0, 16);
const materialized = String(snap.projected.body);
const nestedHead = snap.mixed.nested.projected_title;
snap.mixed.count = snap.mixed.count + 1;
await perf.finish({ id: snap.id, normal_title: snap.normal.title, head: head, materialized_len: materialized.length, nested_head: nestedHead, count: snap.mixed.count, tags: snap.normal.tags.join(",") });
"#.to_string()),
        ("continue_as_seed_host_environment", format!("{CONTEXT}{PROJECTED}{}", r#"
const agent = perf.spawnChild({ task: "inspect carry-forward", capability: "explore" });
const handles = await processes.list({});
const frame = await perf.continueAs({ task: "continue from compact state", seed: { projected_problem: proj.text, nested_projected: { body: proj.json }, computed_summary: `${ctx.user}:${history.length}`, live_agent: handles[0], started_agent: "agent" } });
await agent;
await perf.finish({ frame_key: frame.frame_key, task: frame.task, seed_keys: frame.seed_keys, projected_count: frame.projected_count, global_count: frame.global_count });
"#)),
        ("syntax_text_host_environment", r#"
const patch = "*** Begin Patch\n*** Update File: crates/lash-vm/src/lib.rs\n@@\n-old\n+new\n\\n { braces stay raw }\n*** End Patch";
const script = "python3 - <<'PY'\nprint(\"\"\"double quotes are preserved\"\"\")\n\\n { braces stay raw }\nPY";
const plain = "first\n\"quoted\"\nsecond";
"bare expression branch";
const pieces = [patch.length, patch.includes("*** Begin Patch"), script.startsWith("python3"), script.trim().endsWith("PY"), plain.split("\n").length, plain.slice(0, 5)];
await perf.finish({ patch_head: patch.slice(0, 15), script_head: script.slice(0, 7), plain_lines: plain.split("\n").length, pieces: pieces });
"#.to_string()),
        ("integer_range_host_environment", r#"
const items: number[] = [];
for (let v = -8; v < 9; v++) { items.push(v); }
const forward: number[] = [];
for (let v = 0; v < 10; v += 3) { forward.push(v); }
const backward: number[] = [];
for (let v = 7; v > -3; v -= 2) { backward.push(v); }
const stride = Math.ceil(items.length / 4);
const starts: number[] = [];
const windows: number[][] = [];
for (let i = 0; i < items.length; i += stride) { starts.push(i); windows.push(items.slice(i, i + stride)); }
const text = "alpha beta gamma beta delta";
const firstBeta = text.indexOf("beta");
const secondBeta = text.indexOf("beta", firstBeta + 1);
await perf.finish({ count: items.length, first: items[0], last: items[items.length - 1], forward: forward, backward: backward, stride: stride, starts: starts, windows: windows, mid: items.slice(2, -2), head: text.slice(0, 5), tail: text.slice(-5), first_beta: firstBeta, second_beta: secondBeta, ceil_neg: Math.ceil(-10 / 3), floor_neg: Math.floor(-10 / 3) });
"#.to_string()),
        ("fanout_expression_host_environment", format!("{CONTEXT}{}", r#"
const left = await perf.echo("left");
const right = await perf.echo("right");
const computed = history.length + 39;
const discarded = ["branch_a", 40 + 2, history.length];
const batchedResults = await Promise.all([perf.echo(left), perf.echo(right)]);
const batched = { first: batchedResults[0], second: batchedResults[1], computed: computed };
await perf.finish({ left: left, right: right, computed: computed, discarded: discarded, first: batched.first, second: batched.second, batched_computed: batched.computed });
"#)),
        ("image_host_environment", r#"
const img = { type: "image", id: "img-1", media_type: "image/png", label: "chart.png", size: 1234, width: 640, height: 480 };
const descriptor = JSON.stringify(img);
const metadata = { id: img.id, label: img.label, size: img.size, width: img.width, height: img.height };
console.log(img);
await perf.finish({ metadata: metadata, descriptor_has_type: descriptor.includes("\"type\":\"image\""), descriptor_has_id: descriptor.includes("\"id\":\"img-1\""), dims: `${img.width}x${img.height}`, size_bucket: Math.floor(img.size / 100) });
"#.to_string()),
        ("heap_list_iteration", r#"
const rows: number[] = [];
for (let n = 0; n < 2000; n++) { rows.push(n); }
let total = 0;
let seen = 0;
for (const row of rows) { total = total + row; seen = seen + 1; }
await perf.finish({ total: total, seen: seen });
"#.to_string()),
        ("heap_nested_loop", r#"
const rows: number[][] = [];
let checksum = 0;
for (let n = 0; n < 60; n++) {
  rows.push([n, n + 1]);
  for (const row of rows) { checksum = checksum + row[0]; }
}
await perf.finish({ checksum: checksum, rows: rows.length });
"#.to_string()),
        ("heap_allocation_churn", r#"
type Scratch = { index: number; pair: number[]; label: string };
const kept: Scratch[] = [];
for (let n = 0; n < 400; n++) {
  const scratch: Scratch = { index: n, pair: [n, n + 1], label: `row-${n}` };
  if (n % 40 === 0) { kept.push(scratch); }
}
await perf.finish({ kept: kept.length });
"#.to_string()),
        ("heap_deep_chain_mutation", r#"
const tree = { level: { rows: [[0], [1], [2]], counters: { c: 0 } } };
for (let n = 0; n < 150; n++) {
  tree.level.rows[n % 3] = [n];
  tree.level.counters["c"] = (tree.level.counters["c"] || 0) + 1;
}
await perf.finish({ counter: tree.level.counters["c"], first: tree.level.rows[0] });
"#.to_string()),
        ("heap_comprehension_build", r#"
const source: number[] = [];
for (let n = 0; n < 800; n++) { source.push(n); }
const doubled = source.map((item: number) => item + item);
const tagged: number[][] = [];
for (const item of source) { if (item % 7 === 0) { tagged.push([item]); } }
await perf.finish({ doubled: doubled.length, tagged: tagged.length, last: doubled[doubled.length - 1] });
"#.to_string()),
        ("heap_variable_concat", r#"
const other = [1, 2];
const acc: number[] = [];
for (let n = 0; n < 300; n++) { for (const x of other) { acc.push(x); } }
await perf.finish({ total: acc.length, head: acc[0] });
"#.to_string()),
        // The costliest model-written cell the code mode standard instruction
        // budget was measured over (`InstructionBound::standard`).
        ("rows_print_3000", r#"
const rows = [];
for (let i = 0; i < 3000; i++) {
  rows.push({ index: i, text: "a row the cell prints and finishes with" });
}
console.log(rows);
await perf.finish({ rows });
"#.to_string()),
    ];
    programs.push(("heap_shallow_chain_mutation", chain(6)));
    programs.push(("heap_deep_chain_mutation_24", chain(24)));
    programs
}

/// The production RLM scenario, which is also M9's `production_rlm`.
fn tool_control() -> String {
    r#"
const first = perf.spawnChild({ task: "inspect auth", capability: "explore" });
const second = perf.spawnChild({ task: "inspect api", capability: "explore" });
const llm = perf.queryLlm({ prompt: "summarize benchmark", model: "gpt-5.4-mini" });
const probe = perf.echo("app log");
const handles = await processes.list({});
const results = await Promise.all([first, second, llm, probe]);
await perf.finish({ first: results[0].claim, second: results[1].claim, llm: results[2].text, probe: results[3], tools: handles.length });
"#
    .to_string()
}

/// A tree `depth` records deep whose leaf is written 150 times, built from
/// named records because one literal that deep passes the source nesting
/// limit.
fn chain(depth: usize) -> String {
    let mut source = format!("\nconst t{depth} = {{ leaf: [0] }};\n");
    for level in (0..depth).rev() {
        let inner = level + 1;
        source.push_str(&format!("const t{level} = {{ next: t{inner} }};\n"));
    }
    // Each write walks the whole path, split in two so that no member chain
    // passes the nesting limit either.
    let half = ".next".repeat(depth / 2);
    let rest = ".next".repeat(depth - depth / 2);
    source.push_str(&format!(
        "const tree = t0;\nfor (let n = 0; n < 150; n++) {{ const middle = tree{half}; middle{rest}.leaf = [n]; }}\nconst end = tree{half};\nawait perf.finish({{ leaf: end{rest}.leaf }});\n"
    ));
    source
}

/// Every tool the programs call, each taking one argument.
pub(crate) fn effects() -> BTreeMap<EffectName, Signature> {
    [
        "perf.echo",
        "perf.boom",
        "perf.finish",
        "perf.spawn_child",
        "perf.query_llm",
        "perf.continue_as",
        "processes.list",
        "jobs.run",
    ]
    .into_iter()
    .map(|name| {
        let signature = Signature {
            params: vec![Param {
                name: Name::new("x"),
                ty: Type::Any,
                optional: false,
            }],
            result: Type::Any,
        };
        let effect = EffectName::new(name).unwrap_or_else(|error| panic!("{name}: {error:?}"));
        (effect, signature)
    })
    .collect()
}

fn field<'a>(datum: &'a Datum, name: &str) -> Option<&'a Datum> {
    match datum {
        Datum::Record(fields) => fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value),
        _ => None,
    }
}

fn text(value: &str) -> Datum {
    Datum::Text(value.to_string())
}

/// The scripted answer to one tool call.
pub(crate) fn answer(effect: &str, argument: Datum) -> Outcome {
    match effect {
        "perf.boom" => Outcome::Failed(ErrorDatum {
            kind: "boom".to_string(),
            message: match &argument {
                Datum::Text(message) => message.clone(),
                other => format!("{other:?}"),
            },
            data: Datum::Null,
        }),
        "perf.spawn_child" => {
            let task = match field(&argument, "task") {
                Some(Datum::Text(task)) => task.clone(),
                _ => String::new(),
            };
            Outcome::Completed(Datum::Record(vec![(
                "claim".to_string(),
                Datum::Text(format!("done:{task}")),
            )]))
        }
        "perf.query_llm" => Outcome::Completed(Datum::Record(vec![
            ("text".to_string(), text("benchmark summary")),
            ("tokens".to_string(), Datum::Float(Float::new(42.0))),
        ])),
        "processes.list" => Outcome::Completed(Datum::List(vec![text("proc-1"), text("proc-2")])),
        "jobs.run" => Outcome::Completed(Datum::Record(vec![
            ("exit_code".to_string(), Datum::Float(Float::new(0.0))),
            ("done".to_string(), Datum::Bool(true)),
        ])),
        "perf.continue_as" => {
            let seed_keys = match field(&argument, "seed") {
                Some(Datum::Record(fields)) => fields.iter().map(|(key, _)| text(key)).collect(),
                _ => Vec::new(),
            };
            Outcome::Completed(Datum::Record(vec![
                ("frame_key".to_string(), text("frame-1")),
                (
                    "task".to_string(),
                    field(&argument, "task").cloned().unwrap_or(Datum::Null),
                ),
                ("seed_keys".to_string(), Datum::List(seed_keys)),
                ("projected_count".to_string(), Datum::Float(Float::new(2.0))),
                ("global_count".to_string(), Datum::Float(Float::new(3.0))),
            ]))
        }
        "perf.finish" => Outcome::Completed(Datum::Null),
        _ => Outcome::Completed(argument),
    }
}
