//! The kernel's speed gate (kernel workflow spec §11, gate 6).
//!
//! The programs of the retired VM's benchmark (`crates/lash-vm/benches/
//! benchmark.rs` at `515a596747`, the last main commit before the cutover),
//! written as the TypeScript a cell author would write and lowered through
//! this dialect. Each is timed on the kernel machine alone: lowering,
//! registry assembly and the library's preparation happen once, outside the
//! timed loop, as the old `compiled_execute` mode compiled once. A scripted host answers every
//! tool call at once.
//!
//! `kernel_perf <most iterations> [program or file.ts...]` runs each program
//! for about two seconds and prints one tab-separated row: the manifest's
//! function count, the time per run with and without `start`, the run's
//! charge, and where it went. The split comes from a second run that stops
//! after every unit of charge and reads the pending statement's site from the
//! exported state (exporting changes no charge, `K-MACH-007`): the charge and
//! the time of each step go to TypeScript helper bodies, kernel library bodies
//! or the document's own code. A helper's body is charged as it runs and
//! its formula is not (`K-CHG-007`), so a step is a statement of the body
//! that does the work;
//! the body of a library function with a native implementation is covered
//! by its formula and runs within one step. `KERNEL_PERF_SPLIT=0` skips the
//! second run, for a profiler.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use lash_kernel_dialect::{EffectControl, Environment, NamedLibrary};
use lash_kernel_doc::{
    Datum, EffectName, ErrorDatum, Float, FunctionRegistry, Handle, Integer, Name, Param,
    Signature, Timestamp, Type, Unit,
};
use lash_kernel_vm::{
    Bindings, Bounds, End, Host, KernelMachine, Machine, Outcome, PreparedLibrary, Program,
    Request, Start, Step, Target,
};

const BOUNDS: Bounds = Bounds {
    charge: 1_000_000_000,
    memory: 1 << 30,
    call_depth: 200,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

/// The session data the old benchmark seeded or projected, as the literals a
/// cell would hold.
const CONTEXT: &str = r#"const history = ["alpha", "beta", "gamma"];
const ctx = { user: "sam", attempt: 3 };
"#;

const PROJECTED: &str = r#"const docs = { title: "Authoring Rules", summary: "Rules for authoring", body: "lazy markdown body" };
const proj = { items: ["alpha", "beta", "gamma", "delta"], text: "alpha beta gamma delta", padded: "  alpha  ", json: "{\"ok\":true,\"count\":4}", number: "4", record: { topic: "bench", count: 4 } };
"#;

fn programs() -> Vec<(&'static str, String)> {
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
const lookup = echo(labels.join(","));
const stats = echo({ total: total, count: items.length, seen: history.length, index_count: indexes.length });
const fanout = await Promise.all([lookup, stats]);
const lookupValue = fanout[0];
const statsValue = fanout[1];
await finish(`user=${ctx.user};attempt=${ctx.attempt};active=${lookupValue};total=${statsValue.total};count=${statsValue.count};seen=${statsValue.seen};indexes=${statsValue.index_count}`);
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
const positionalResults = await Promise.all([echo(`left:${tokens[0]}`), echo(tokens[1]), echo(tokens.length)]);
const positional = [positionalResults[0], positionalResults[1], positionalResults[2]];
const namedResults = await Promise.all([echo(source), echo(`${trimmedUser}:${count}`)]);
const named = { lookup: namedResults[0], summary: namedResults[1] };
const handle = echo("awaited");
const awaited = await handle;
const direct = await echo(trimmedUser);
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
await finish({ direct: direct, awaited: awaited, positional: positional, lookup: named.lookup, summary: named.summary, values: Object.values(state.counts), beta_index: betaIndex, line_matches: lineMatches, first_two: state.tags.slice(0, 2), validated: validated, stringified: JSON.stringify(validated) });
"#)),
        ("async_await", r#"
const handles = [echo("alpha"), echo("beta"), echo("gamma")];
const results = await Promise.all(handles);
await finish([results[0], results[1], results[2]].join(","));
"#.to_string()),
        ("direct_unwrap", r#"
const first = await echo("alpha");
const second = await echo(`${first}:beta`);
const third = await echo([first, second].join(","));
await finish(third);
"#.to_string()),
        ("general_fanout", r#"
const seed = ["alpha", "beta", "gamma"];
const results = await Promise.all([echo(`${seed[0]}:${seed.length}`), echo(`${seed[1]}:${seed.length}`)]);
await finish(`${results[0]}|${results[1]}`);
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
await finish({ kept: kept, skipped: skipped, outer: outer });
"#.to_string()),
        ("indexed_assignment", r#"
const groups = ["alpha", "beta", "alpha", "gamma", "beta", "alpha", "delta", "gamma"];
const counts = {};
for (const group of groups) { counts[group] = (counts[group] || 0) + 1; }
const state = { groups: { alpha: { count: 0 }, beta: { count: 0 }, gamma: { count: 0 }, delta: { count: 0 } } };
for (const group of Object.keys(counts)) { state.groups[group].count = counts[group]; }
const summary: string[] = [];
for (const group of Object.keys(counts)) { summary.push(`${group}:${state.groups[group].count}`); }
await finish({ counts: counts, state: state, summary: summary.join(",") });
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
await finish({ history_len: history.length, first_role: first.role, first_content: first.content, second_content: second.content, doc_title: docs.title, doc_summary: docs.summary, body_head: bodyHead, body_match_index: bodyMatchIndex, body_matches: bodyMatches, second_matches: secondMatches, body_truthy: bodyTruthy, body_text: docs.body });
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
await finish({ count: 512, total: total, groups: groups, evens: evens.length, odds: odds.length, summary: lines.join("|") });
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
await finish([validated[0].value, validated[1].value, validated[2].value, validated[3].value, validated[4].value, validated[5].value].join("|"));
"#)),
        ("projected_operations", format!("{PROJECTED}{}", r#"
const pushed = proj.items.slice(0);
pushed.push("epsilon");
await finish({ len: proj.items.length, empty: proj.items.length === 0, keys: Object.keys(proj.record), values: Object.values(proj.record), contains: proj.items.includes("beta"), starts: proj.text.startsWith("alpha"), ends: proj.text.endsWith("delta"), split_count: proj.text.split(" ").length, join: proj.items.join(","), trim: proj.padded.trim(), slice_text: proj.text.slice(6, 10), slice_list: proj.items.slice(1, 3), pushed: pushed, as_int: Math.trunc(Number(proj.number)), as_float: Number(proj.number), parsed: JSON.parse(proj.json), first: proj.items[0], field: proj.record.topic });
"#)),
        ("type_system_stress", format!("{CONTEXT}{}", r#"
type Meta = { source: string; attempt: number; tags: string[] };
type Item = { id: number; title: string; score: number; active: boolean; meta: Meta; maybe: string | null };
const items: Item[] = [];
for (let i = 0; i < 64; i++) {
  const raw: Item = { id: i, title: `item-${i}`, score: i / 2, active: i % 2 === 0, meta: { source: ctx.user, attempt: ctx.attempt, tags: ["alpha", "beta", "gamma"] }, maybe: i % 3 === 0 ? null : `v${i}` };
  items.push(raw);
}
await finish({ count: items.length, first: items[0].title, last: items[63].title, tags: items[1].meta.tags.join(",") });
"#)),
        ("wrapped_error_paths", r#"
let missingOk = true;
let missingError = false;
try { await boom("unknown tool missing_tool"); } catch (error) { missingOk = false; missingError = String(error.message).includes("unknown tool"); }
let boomOk = true;
let boomError = false;
try { await boom("explicit failure"); } catch (error) { boomOk = false; boomError = String(error.message).includes("explicit failure"); }
const ok = await echo("still-running");
const probe = await jobs.run({ target: "check Cargo.lock" });
await finish({ missing_ok: missingOk, missing_error: missingError, boom_ok: boomOk, boom_error: boomError, ok_value: ok, probe_exit: probe.exit_code, probe_done: probe.done });
"#.to_string()),
        ("tool_control_host_environment", tool_control()),
        ("snapshot_projected_state", r#"
const snap = { id: "snapshot-mixed", normal: { title: "Snapshot Rules", tags: ["snapshot", "projected", "mixed"] }, projected: { body: "projected body stays lazy across snapshot markers" }, mixed: { count: 7, nested: { projected_title: "Nested Projection" } } };
const head = snap.projected.body.slice(0, 16);
const materialized = String(snap.projected.body);
const nestedHead = snap.mixed.nested.projected_title;
snap.mixed.count = snap.mixed.count + 1;
await finish({ id: snap.id, normal_title: snap.normal.title, head: head, materialized_len: materialized.length, nested_head: nestedHead, count: snap.mixed.count, tags: snap.normal.tags.join(",") });
"#.to_string()),
        ("continue_as_seed_host_environment", format!("{CONTEXT}{PROJECTED}{}", r#"
const agent = spawn_child({ task: "inspect carry-forward", capability: "explore" });
const handles = await processes.list({});
const frame = await continue_as({ task: "continue from compact state", seed: { projected_problem: proj.text, nested_projected: { body: proj.json }, computed_summary: `${ctx.user}:${history.length}`, live_agent: handles[0], started_agent: "agent" } });
await agent;
await finish({ frame_key: frame.frame_key, task: frame.task, seed_keys: frame.seed_keys, projected_count: frame.projected_count, global_count: frame.global_count });
"#)),
        ("syntax_text_host_environment", r#"
const patch = "*** Begin Patch\n*** Update File: crates/lash-vm/src/lib.rs\n@@\n-old\n+new\n\\n { braces stay raw }\n*** End Patch";
const script = "python3 - <<'PY'\nprint(\"\"\"double quotes are preserved\"\"\")\n\\n { braces stay raw }\nPY";
const plain = "first\n\"quoted\"\nsecond";
"bare expression branch";
const pieces = [patch.length, patch.includes("*** Begin Patch"), script.startsWith("python3"), script.trim().endsWith("PY"), plain.split("\n").length, plain.slice(0, 5)];
await finish({ patch_head: patch.slice(0, 15), script_head: script.slice(0, 7), plain_lines: plain.split("\n").length, pieces: pieces });
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
await finish({ count: items.length, first: items[0], last: items[items.length - 1], forward: forward, backward: backward, stride: stride, starts: starts, windows: windows, mid: items.slice(2, -2), head: text.slice(0, 5), tail: text.slice(-5), first_beta: firstBeta, second_beta: secondBeta, ceil_neg: Math.ceil(-10 / 3), floor_neg: Math.floor(-10 / 3) });
"#.to_string()),
        ("fanout_expression_host_environment", format!("{CONTEXT}{}", r#"
const left = await echo("left");
const right = await echo("right");
const computed = history.length + 39;
const discarded = ["branch_a", 40 + 2, history.length];
const batchedResults = await Promise.all([echo(left), echo(right)]);
const batched = { first: batchedResults[0], second: batchedResults[1], computed: computed };
await finish({ left: left, right: right, computed: computed, discarded: discarded, first: batched.first, second: batched.second, batched_computed: batched.computed });
"#)),
        ("image_host_environment", r#"
const img = { type: "image", id: "img-1", media_type: "image/png", label: "chart.png", size: 1234, width: 640, height: 480 };
const descriptor = JSON.stringify(img);
const metadata = { id: img.id, label: img.label, size: img.size, width: img.width, height: img.height };
console.log(img);
await finish({ metadata: metadata, descriptor_has_type: descriptor.includes("\"type\":\"image\""), descriptor_has_id: descriptor.includes("\"id\":\"img-1\""), dims: `${img.width}x${img.height}`, size_bucket: Math.floor(img.size / 100) });
"#.to_string()),
        ("heap_list_iteration", r#"
const rows: number[] = [];
for (let n = 0; n < 2000; n++) { rows.push(n); }
let total = 0;
let seen = 0;
for (const row of rows) { total = total + row; seen = seen + 1; }
await finish({ total: total, seen: seen });
"#.to_string()),
        ("heap_nested_loop", r#"
const rows: number[][] = [];
let checksum = 0;
for (let n = 0; n < 60; n++) {
  rows.push([n, n + 1]);
  for (const row of rows) { checksum = checksum + row[0]; }
}
await finish({ checksum: checksum, rows: rows.length });
"#.to_string()),
        ("heap_allocation_churn", r#"
type Scratch = { index: number; pair: number[]; label: string };
const kept: Scratch[] = [];
for (let n = 0; n < 400; n++) {
  const scratch: Scratch = { index: n, pair: [n, n + 1], label: `row-${n}` };
  if (n % 40 === 0) { kept.push(scratch); }
}
await finish({ kept: kept.length });
"#.to_string()),
        ("heap_deep_chain_mutation", r#"
const tree = { level: { rows: [[0], [1], [2]], counters: { c: 0 } } };
for (let n = 0; n < 150; n++) {
  tree.level.rows[n % 3] = [n];
  tree.level.counters["c"] = (tree.level.counters["c"] || 0) + 1;
}
await finish({ counter: tree.level.counters["c"], first: tree.level.rows[0] });
"#.to_string()),
        ("heap_comprehension_build", r#"
const source: number[] = [];
for (let n = 0; n < 800; n++) { source.push(n); }
const doubled = source.map((item: number) => item + item);
const tagged: number[][] = [];
for (const item of source) { if (item % 7 === 0) { tagged.push([item]); } }
await finish({ doubled: doubled.length, tagged: tagged.length, last: doubled[doubled.length - 1] });
"#.to_string()),
        ("heap_variable_concat", r#"
const other = [1, 2];
const acc: number[] = [];
for (let n = 0; n < 300; n++) { for (const x of other) { acc.push(x); } }
await finish({ total: acc.length, head: acc[0] });
"#.to_string()),
    ];
    programs.push(("heap_shallow_chain_mutation", chain(6)));
    programs.push(("heap_deep_chain_mutation_24", chain(24)));
    programs
}

/// The production RLM scenario, which is also M9's `production_rlm`.
fn tool_control() -> String {
    r#"
const first = spawn_child({ task: "inspect auth", capability: "explore" });
const second = spawn_child({ task: "inspect api", capability: "explore" });
const llm = query_llm({ prompt: "summarize benchmark", model: "gpt-5.4-mini" });
const probe = echo("app log");
const handles = await processes.list({});
const results = await Promise.all([first, second, llm, probe]);
await finish({ first: results[0].claim, second: results[1].claim, llm: results[2].text, probe: results[3], tools: handles.length });
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
        "const tree = t0;\nfor (let n = 0; n < 150; n++) {{ const middle = tree{half}; middle{rest}.leaf = [n]; }}\nconst end = tree{half};\nawait finish({{ leaf: end{rest}.leaf }});\n"
    ));
    source
}

/// Every tool the programs call, each taking one argument.
#[expect(
    clippy::expect_used,
    reason = "benchmark fixture: every tool name is a literal identifier"
)]
fn effects() -> BTreeMap<EffectName, Signature> {
    [
        "echo",
        "boom",
        "finish",
        "spawn_child",
        "query_llm",
        "continue_as",
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
        (EffectName::new(name).expect("a tool's name"), signature)
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
fn answer(effect: &str, argument: Datum) -> Outcome {
    match effect {
        "boom" => Outcome::Failed(ErrorDatum {
            kind: "boom".to_string(),
            message: match &argument {
                Datum::Text(message) => message.clone(),
                other => format!("{other:?}"),
            },
            data: Datum::Null,
        }),
        "spawn_child" => {
            let task = match field(&argument, "task") {
                Some(Datum::Text(task)) => task.clone(),
                _ => String::new(),
            };
            Outcome::Completed(Datum::Record(vec![(
                "claim".to_string(),
                Datum::Text(format!("done:{task}")),
            )]))
        }
        "query_llm" => Outcome::Completed(Datum::Record(vec![
            ("text".to_string(), text("benchmark summary")),
            ("tokens".to_string(), Datum::Float(Float::new(42.0))),
        ])),
        "processes.list" => Outcome::Completed(Datum::List(vec![text("proc-1"), text("proc-2")])),
        "jobs.run" => Outcome::Completed(Datum::Record(vec![
            ("exit_code".to_string(), Datum::Float(Float::new(0.0))),
            ("done".to_string(), Datum::Bool(true)),
        ])),
        "continue_as" => {
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
        "finish" => Outcome::Completed(Datum::Null),
        _ => Outcome::Completed(argument),
    }
}

#[derive(Default)]
struct Console {
    printed: usize,
}

impl Host for Console {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }

    fn random(&mut self) -> u64 {
        0
    }

    fn read(&mut self, _handle: &Handle, _request: &Datum) -> Result<Datum, ErrorDatum> {
        Err(ErrorDatum {
            kind: "type_error".to_string(),
            message: "the benchmark host has no projection".to_string(),
            data: Datum::Null,
        })
    }

    fn print(&mut self, _value: &Datum) {
        self.printed += 1;
    }

    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// Answers every request of a park, in the order asked.
fn deliver_all(machine: &mut KernelMachine, requests: Vec<Request>) -> Option<Datum> {
    let mut finished = None;
    for request in requests {
        let (wait, outcome) = match request {
            Request::Sleep(sleep) => (sleep.wait, Outcome::Elapsed),
            Request::Effect(effect) => {
                let argument = effect.args.into_iter().next().unwrap_or(Datum::Null);
                if effect.effect.as_str() == "finish" {
                    finished = Some(argument.clone());
                }
                (effect.wait, answer(effect.effect.as_str(), argument))
            }
        };
        machine
            .deliver(wait, outcome)
            .unwrap_or_else(|error| panic!("{error}"));
    }
    finished
}

fn start(program: &Program) -> KernelMachine {
    let start = Start {
        target: Target::Main,
        args: Vec::new(),
        bindings: Bindings::default(),
    };
    KernelMachine::start(program.clone(), BOUNDS, start).unwrap_or_else(|error| panic!("{error}"))
}

/// Runs a started machine to its end. Gives the charge and the value the
/// program finished with.
fn to_end(machine: &mut KernelMachine, name: &str) -> (u64, Datum) {
    let mut console = Console::default();
    let mut finished = Datum::Null;
    loop {
        match machine
            .run(&mut console, u64::MAX)
            .unwrap_or_else(|error| panic!("{name}: {error}"))
        {
            Step::Parked(park) => {
                if let Some(value) = deliver_all(machine, park.requests) {
                    finished = value;
                }
            }
            Step::Ended(End::Finished(_)) => return (machine.meters().charged, finished),
            Step::Ended(other) => panic!("{name} ended {other:?}"),
            Step::Slice => unreachable!("the slice is unbounded"),
        }
    }
}

/// Where the charge of one run went.
#[derive(Default)]
struct Split {
    /// Steps of one unit of charge.
    steps: u64,
    helpers: u64,
    library: u64,
    program: u64,
    by_helper: BTreeMap<String, u64>,
    /// Time inside `run`, by where the step ran: helper bodies, and the
    /// rest. Stepping and exporting slow a run down, so only the shares
    /// mean anything.
    helper_time: std::time::Duration,
    other_time: std::time::Duration,
    helper_time_by: BTreeMap<String, std::time::Duration>,
}

/// Runs the program one unit of charge at a time, attributing each step's
/// charge to the body its ready task's pending statement is in.
fn split(program: &Program, registry: &FunctionRegistry, name: &str) -> Split {
    let mut machine = start(program);
    let mut console = Console::default();
    let mut split = Split::default();
    let mut charged = 0;
    loop {
        let unit = pending_unit(&mut machine);
        let began = Instant::now();
        let step = machine
            .run(&mut console, 1)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let took = began.elapsed();
        split.steps += 1;
        let now = machine.meters().charged;
        let spent = now - charged;
        charged = now;
        match unit {
            Some(Unit::Library(function)) => {
                let label = registry
                    .get(&function)
                    .map_or_else(|| function.to_string(), |f| f.definition.name.to_string());
                if label.starts_with("ts.") {
                    split.helpers += spent;
                    split.helper_time += took;
                    *split.helper_time_by.entry(label.clone()).or_default() += took;
                    *split.by_helper.entry(label).or_default() += spent;
                } else {
                    split.library += spent;
                    split.other_time += took;
                }
            }
            _ => {
                split.program += spent;
                split.other_time += took;
            }
        }
        match step {
            Step::Slice => {}
            Step::Parked(park) => {
                deliver_all(&mut machine, park.requests);
            }
            Step::Ended(End::Finished(_)) => return split,
            Step::Ended(other) => panic!("{name} ended {other:?}"),
        }
    }
}

/// The site unit of the statement the next ready task runs next.
fn pending_unit(machine: &mut KernelMachine) -> Option<Unit> {
    let parked = machine.export().ok()?;
    let ready = parked.run.ready.first()?;
    let task = usize::try_from(ready.0).ok()?;
    let call = parked.tasks.get(task)?.calls.last()?;
    Some(call.call.statement.unit.clone())
}

#[expect(
    clippy::expect_used,
    reason = "benchmark setup: the shipped library and helpers register by construction"
)]
fn registry() -> Arc<FunctionRegistry> {
    let mut registry = FunctionRegistry::new();
    lash_kernel_lib::register_numbers(&mut registry).expect("numeric library registration");
    lash_kernel_lib::register_text_json(&mut registry).expect("text library registration");
    lash_kernel_vm::register_machine_functions(&mut registry)
        .expect("machine library registration");
    lash_kernel_lib::register_collections(&mut registry).expect("collection library registration");
    lash_ext_regex_ecma::register(
        &mut registry,
        &Arc::new(lash_ext_regex_ecma::Engine::new(32)),
    )
    .expect("regex extension registration");
    lash_ext_date_ecma::register(&mut registry).expect("date extension registration");
    lash_ext_url_whatwg::register(&mut registry).expect("URL extension registration");
    let mut library = NamedLibrary::from_registry(&registry).expect("unique kernel names");
    for definition in lash_dialect_typescript::define_helpers(&mut library)
        .unwrap_or_else(|error| panic!("{error}"))
    {
        registry
            .register(definition, None)
            .unwrap_or_else(|error| panic!("{error}"));
    }
    Arc::new(registry)
}

#[expect(
    clippy::print_stdout,
    clippy::expect_used,
    clippy::disallowed_methods,
    reason = "benchmark entry point: it reads its arguments, its program files and its one switch, and reports on stdout"
)]
fn main() {
    let mut args = std::env::args().skip(1);
    let cap: u32 = args
        .next()
        .map_or(2000, |count| count.parse().expect("an iteration count"));
    // A name selects a program; a path to a `.ts` file adds one, named by
    // its path.
    let (files, only): (Vec<String>, BTreeSet<String>) = {
        let (files, names): (Vec<String>, Vec<String>) = args.partition(|arg| arg.ends_with(".ts"));
        (files, names.into_iter().collect())
    };
    let mut programs: Vec<(String, String)> = programs()
        .into_iter()
        .filter(|(name, _)| only.is_empty() && files.is_empty() || only.contains(*name))
        .map(|(name, source)| (name.to_string(), source))
        .collect();
    for file in files {
        let source =
            std::fs::read_to_string(&file).unwrap_or_else(|error| panic!("{file}: {error}"));
        programs.push((file, source));
    }
    let registry = registry();
    // Prepared once, as a worker prepares its registry: every library body
    // is compiled here, and a run's start compiles only its document.
    let prepared = PreparedLibrary::new(Arc::clone(&registry));
    let library = NamedLibrary::from_registry(&registry).expect("unique library names");
    let effects = effects();
    let controls = BTreeMap::from([(
        EffectName::new("finish").expect("a tool's name"),
        BTreeSet::from([EffectControl::Finish]),
    )]);
    let tool_roots: BTreeSet<Name> = ["processes", "jobs"].into_iter().map(Name::new).collect();
    let environment = Environment {
        library: &library,
        effects: &effects,
        tool_roots: &tool_roots,
        controls: &controls,
        bindings: &BTreeSet::new(),
        functions: &BTreeMap::new(),
    };
    println!(
        "program\tfunctions\titerations\tns_start_and_run\tns_run\tcharged\tsteps\thelper_charge\tlibrary_body_charge\tprogram_charge\ttop_helpers\thelper_time_share\tslowest_helpers\tresult"
    );
    for (name, source) in &programs {
        let name = name.as_str();
        let lowered = match lash_dialect_typescript::lower(source, &environment) {
            Ok(lowered) => lowered,
            Err(diagnostic) => {
                println!("{name}\trefused\t{diagnostic:?}");
                continue;
            }
        };
        let functions = lowered.document.manifest.functions.len();
        let program = Program {
            document: Arc::new(lowered.document),
            library: prepared.clone(),
        };
        let first = Instant::now();
        let (charged, result) = to_end(&mut start(&program), name);
        // About two seconds a program, and at most the requested count.
        let once = first.elapsed().as_secs_f64().max(1e-6);
        let iterations = ((2.0 / once) as u32).clamp(3, cap);
        for _ in 0..iterations.min(20) {
            to_end(&mut start(&program), name);
        }
        let mut running = std::time::Duration::ZERO;
        let began = Instant::now();
        for _ in 0..iterations {
            let mut machine = start(&program);
            let ran = Instant::now();
            to_end(&mut machine, name);
            running += ran.elapsed();
        }
        let whole = began.elapsed();
        let per = |total: std::time::Duration| total.as_nanos() as f64 / f64::from(iterations);
        if std::env::var("KERNEL_PERF_SPLIT").is_ok_and(|split| split == "0") {
            println!(
                "{name}\t{functions}\t{iterations}\t{:.1}\t{:.1}\t{charged}",
                per(whole),
                per(running),
            );
            continue;
        }
        let split = split(&program, &registry, name);
        assert_eq!(
            split.helpers + split.library + split.program,
            charged,
            "{name}: a stepped run charges what a whole run charges"
        );
        let mut top: Vec<_> = split.by_helper.iter().collect();
        top.sort_by(|a, b| b.1.cmp(a.1));
        let top = top
            .iter()
            .take(5)
            .map(|(helper, charge)| format!("{helper}={charge}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut slow: Vec<_> = split.helper_time_by.iter().collect();
        slow.sort_by(|a, b| b.1.cmp(a.1));
        let slow = slow
            .iter()
            .take(5)
            .map(|(helper, took)| format!("{helper}={}us", took.as_micros()))
            .collect::<Vec<_>>()
            .join(",");
        let stepped = split.helper_time + split.other_time;
        let helper_time_share = split.helper_time.as_secs_f64() / stepped.as_secs_f64().max(1e-9);
        println!(
            "{name}\t{functions}\t{iterations}\t{:.1}\t{:.1}\t{charged}\t{}\t{}\t{}\t{}\t{top}\t{helper_time_share:.2}\t{slow}\t{result:?}",
            per(whole),
            per(running),
            split.steps,
            split.helpers,
            split.library,
            split.program,
        );
    }
}
