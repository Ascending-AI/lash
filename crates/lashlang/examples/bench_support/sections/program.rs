const BENCH_PROCESS_DECLS: &str = r#"
process echo(value: any) {
  finish value
}

process spawn_child(task: str, capability: str) {
  finish { claim: format("done:{0}", task) }
}

process query_llm(prompt: str, model: str) {
  finish { text: "benchmark summary", tokens: 42 }
}
"#;

pub fn benchmark_program(scenario: Scenario) -> String {
    let main = match scenario {
        Scenario::Baseline => {
            r#"
items = [
  { label: "alpha", weight: 1, active: true },
  { label: "beta", weight: 2, active: false },
  { label: "gamma", weight: 3, active: true }
]
indexes = range(0, len(items))
all_indexes = push(indexes, len(items))
total = 0
labels = []
for item in items {
  total = total + item.weight
  if item.active {
    labels = labels + [format("{0}:{1}", item.label, item.weight)]
  }
}
lookup_handle = start echo(value: join(labels, ","))
stats_handle = start echo(value: { total: total, count: len(items), seen: len(history), index_count: len(all_indexes) })
fanout = await { lookup: lookup_handle, stats: stats_handle }
lookup_value = fanout.lookup?
stats_value = validate(fanout.stats?, Type { total: int, count: int, seen: int, index_count: int })
summary = format(
  "user={0};attempt={1};active={2};total={3};count={4};seen={5};indexes={6}",
  ctx.user,
  ctx.attempt,
  lookup_value,
  stats_value.total,
  stats_value.count,
  stats_value.seen,
  stats_value.index_count
)
finish summary
"#
        }
        Scenario::LanguageHostEnvironment => {
            r#"
source = join(history, ",")
tokens = split(source, ",")
trimmed_user = trim(format(" {0} ", ctx.user))
beta_index = find(source, "beta")
line_matches = grep_text(join(tokens, "\n"), "a")
count = len(tokens)
empty_tail = empty(slice(tokens, count, count))
predicates = [
  contains(source, tokens[1]),
  starts_with(source, tokens[0]),
  ends_with(source, tokens[2])
]
numeric = {
  neg: -ctx.attempt,
  sum: ctx.attempt + count,
  diff: count - 1,
  product: count * 2,
  quotient: count / 2,
  modulo: count % 2,
  parsed_int: to_int(ctx.attempt),
  parsed_float: to_float(ctx.attempt)
}
logic = not false and (count > 2 or empty_tail)
comparisons = [
  count == 3,
  count != 4,
  count < 4,
  count <= 3,
  count > 2,
  count >= 3
]
choice = logic ? "yes" : "no"
json_text = "{\"attempt\":" + to_string(ctx.attempt) + ",\"ok\":true}"
parsed = json_parse(json_text)
positional_results = await [
  start echo(value: format("left:{0}", tokens[0])),
  start echo(value: tokens[1]),
  start echo(value: len(tokens))
]
positional = [positional_results[0]?, positional_results[1], positional_results[2]?]
named_results = await {
  lookup: start echo(value: source),
  summary: start echo(value: format("{0}:{1}", trimmed_user, count))
}
named = { lookup: named_results.lookup, summary: named_results.summary? }
cancelled = start echo(value: "cancelled")
cancel cancelled
handle = start echo(value: "awaited")
awaited = (await handle)?
direct = await tools.echo({ value: trimmed_user })?
print direct
state = {
  tags: push(tokens, "delta"),
  counts: {},
  kept: [],
  predicates: predicates,
  comparisons: comparisons,
  numeric: numeric,
  parsed: parsed,
  line_matches: line_matches
}
state.tags[1] = "beta"
for token in state.tags {
  state.counts[token] = state.counts[token] + 1
}
for token in keys(state.counts) {
  if token == "beta" {
    continue
  }
  if token == "delta" {
    break
  }
  state.kept = push(state.kept, token)
}
Payload = Type {
  user: str,
  choice: enum["yes", "no"],
  tags: list[str],
  counts: dict,
  kept: list[str],
  maybe: str | null,
  optional_note: str?
}
validated = validate(
  {
    user: direct,
    choice: choice,
    tags: state.tags,
    counts: state.counts,
    kept: state.kept,
    maybe: null
  },
  Payload
)
finish {
  direct: direct,
  awaited: awaited,
  positional: positional,
  lookup: named.lookup?,
  summary: named.summary,
  values: values(state.counts),
  beta_index: beta_index,
  line_matches: line_matches,
  first_two: slice(state.tags, null, 2),
  validated: validated,
  stringified: to_string(validated)
}
"#
        }
        Scenario::AsyncAwait => {
            r#"
handles = {
  alpha: start echo(value: "alpha"),
  beta: start echo(value: "beta"),
  gamma: start echo(value: "gamma")
}
results = await handles
formatted = [results.alpha?, results.beta?, results.gamma?]
finish join(formatted, ",")
"#
        }
        Scenario::DirectUnwrap => {
            r#"
first = await tools.echo({ value: "alpha" })?
second = await tools.echo({ value: format("{0}:{1}", first, "beta") })?
third = await tools.echo({ value: join([first, second], ",") })?
finish third
"#
        }
        Scenario::GeneralFanout => {
            r#"
seed = ["alpha", "beta", "gamma"]
results = await {
  left: start echo(value: format("{0}:{1}", seed[0], len(seed))),
  right: start echo(value: format("{0}:{1}", seed[1], len(seed)))
}
finish format("{0}|{1}", results.left?, results.right?)
"#
        }
        Scenario::LoopControl => {
            r#"
items = range(0, 128)
outer = "restored"
kept = 0
skipped = 0
for outer in items {
  if outer < 32 {
    skipped = skipped + 1
    continue
  }
  if outer >= 96 {
    break
  }
  if outer % 3 == 0 {
    continue
  }
  kept = kept + 1
}
finish { kept: kept, skipped: skipped, outer: outer }
"#
        }
        Scenario::IndexedAssignment => {
            r#"
groups = ["alpha", "beta", "alpha", "gamma", "beta", "alpha", "delta", "gamma"]
counts = {}
for group in groups {
  counts[group] = counts[group] + 1
}
state = { groups: { alpha: { count: 0 }, beta: { count: 0 }, gamma: { count: 0 }, delta: { count: 0 } } }
for group in keys(counts) {
  state.groups[group].count = counts[group]
}
summary = []
for group in keys(counts) {
  summary = summary + [format("{0}:{1}", group, state.groups[group].count)]
}
finish { counts: counts, state: state, summary: join(summary, ",") }
"#
        }
        Scenario::ProjectedValues => {
            r#"
first = history[0]
second = history[1]
body_head = docs.body[0]
body_match_index = find(docs.body, "markdown")
body_matches = grep_text(docs.body, "markdown")
second_matches = grep_text(second.content, "response")
if docs.body {
  body_truthy = true
} else {
  body_truthy = false
}
print docs.body
summary = {
  history_len: len(history),
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
}
finish summary
"#
        }
        Scenario::LargeData => {
            r#"
items = range(0, 512)
groups = {}
total = 0
evens = []
odds = []
for item in items {
  key = format("bucket_{0}", item % 16)
  groups[key] = groups[key] + 1
  total = total + item
  if item % 2 == 0 {
    evens = push(evens, item)
  } else {
    odds = push(odds, item)
  }
}
lines = []
for key in keys(groups) {
  lines = push(lines, format("{0}:{1}", key, groups[key]))
}
payload = {
  count: len(items),
  total: total,
  groups: groups,
  evens: len(evens),
  odds: len(odds),
  summary: join(lines, "|")
}
finish validate(payload, Type {
  count: int,
  total: int,
  groups: dict,
  evens: int,
  odds: int,
  summary: str
})
"#
        }
        Scenario::CachePressure => {
            r#"
seed = {
  user: ctx.user,
  attempt: ctx.attempt,
  history_len: len(history),
  labels: ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"]
}
a0 = format("{0}:{1}", seed.labels[0], seed.attempt)
a1 = format("{0}:{1}", seed.labels[1], seed.history_len)
a2 = format("{0}:{1}", seed.labels[2], len(a0))
a3 = format("{0}:{1}", seed.labels[3], len(a1))
a4 = format("{0}:{1}", seed.labels[4], len(a2))
a5 = format("{0}:{1}", seed.labels[5], len(a3))
r0 = { name: "r0", value: a0, next: a1 }
r1 = { name: "r1", value: a1, next: a2 }
r2 = { name: "r2", value: a2, next: a3 }
r3 = { name: "r3", value: a3, next: a4 }
r4 = { name: "r4", value: a4, next: a5 }
r5 = { name: "r5", value: a5, next: a0 }
TypeA = Type { name: str, value: str, next: str }
validated = [
  validate(r0, TypeA),
  validate(r1, TypeA),
  validate(r2, TypeA),
  validate(r3, TypeA),
  validate(r4, TypeA),
  validate(r5, TypeA)
]
finish join([
  validated[0].value,
  validated[1].value,
  validated[2].value,
  validated[3].value,
  validated[4].value,
  validated[5].value
], "|")
"#
        }
        Scenario::ProjectedOperations => {
            r#"
summary = {
  len: len(proj.items),
  empty: empty(proj.items),
  keys: keys(proj.record),
  values: values(proj.record),
  contains: contains(proj.items, "beta"),
  starts: starts_with(proj.text, "alpha"),
  ends: ends_with(proj.text, "delta"),
  split_count: len(split(proj.text, " ")),
  join: join(proj.items, ","),
  trim: trim(proj.padded),
  slice_text: slice(proj.text, 6, 10),
  slice_list: slice(proj.items, 1, 3),
  pushed: push(proj.items, "epsilon"),
  as_int: to_int(proj.number),
  as_float: to_float(proj.number),
  parsed: json_parse(proj.json),
  first: proj.items[0],
  field: proj.record.topic
}
finish summary
"#
        }
        Scenario::TypeSystemStress => {
            r#"
Meta = Type {
  source: str,
  attempt: int,
  tags: list[enum["alpha", "beta", "gamma", "delta"]],
  optional_note: str?
}
Item = Type {
  id: int,
  title: str,
  score: float,
  active: bool,
  meta: Meta,
  maybe: str | int | null
}
items = []
for i in range(0, 64) {
  raw = {
    id: i,
    title: format("item-{0}", i),
    score: i / 2,
    active: i % 2 == 0,
    meta: {
      source: ctx.user,
      attempt: ctx.attempt,
      tags: ["alpha", "beta", "gamma"]
    },
    maybe: i % 3 == 0 ? null : format("v{0}", i)
  }
  items = push(items, validate(raw, Item))
}
finish {
  count: len(items),
  first: items[0].title,
  last: items[63].title,
  tags: join(items[1].meta.tags, ",")
}
"#
        }
        Scenario::WrappedErrorPaths => {
            r#"
missing = await tools.missing_tool({ value: "x" })
boom = await tools.boom({ reason: "explicit" })
ok = await tools.echo({ value: "still-running" })
probe = await shell.exec({ cmd: "test -f Cargo.lock" })
finish {
  missing_ok: missing.ok,
  missing_error: contains(missing.error, "unknown tool"),
  boom_ok: boom.ok,
  boom_error: contains(boom.error, "explicit failure"),
  ok_value: ok.value,
  probe_exit: probe.value.exit_code,
  probe_done: probe.value.done
}
"#
        }
        Scenario::ToolControlHostEnvironment => {
            r#"
first = start spawn_child(task: "inspect auth", capability: "explore")
second = start spawn_child(task: "inspect api", capability: "explore")
llm = start query_llm(prompt: "summarize benchmark", model: "gpt-5.4-mini")
probe = start echo(value: "app log")
handles = await processes.list({})?
results = await {
  first: first,
  second: second,
  llm: llm,
  probe: probe
}
cancel second
finish {
  first: results.first.value.claim,
  second: results.second.value.claim,
  llm: results.llm.value.text,
  probe: results.probe.value,
  tools: len(handles)
}
"#
        }
        Scenario::SnapshotProjectedState => {
            r#"
head = slice(snap.projected.body, 0, 16)
materialized = to_string(snap.projected.body)
nested_head = snap.mixed.nested.projected_title
snap.mixed.count = snap.mixed.count + 1
finish {
  id: snap.id,
  normal_title: snap.normal.title,
  head: head,
  materialized_len: len(materialized),
  nested_head: nested_head,
  count: snap.mixed.count,
  tags: join(snap.normal.tags, ",")
}
"#
        }
        Scenario::ContinueAsSeedHostEnvironment => {
            r#"
agent = start spawn_child(task: "inspect carry-forward", capability: "explore")
handles = await processes.list({})?
frame = await control.continue_as({
  task: "continue from compact state",
    seed: {
      projected_problem: proj.text,
      nested_projected: { body: proj.json },
      computed_summary: format("{0}:{1}", ctx.user, len(history)),
      live_agent: handles[0],
      started_agent: agent
    }
  })?
finish {
  frame_id: frame.frame_id,
  task: frame.task,
  seed_keys: frame.seed_keys,
  projected_count: frame.projected_count,
  global_count: frame.global_count
}
"#
        }
        Scenario::TriggerRegistryHostEnvironment => {
            r#"
process daily_digest(tick: cron.Tick) {
  finish { kind: "daily_digest", fired_at: tick.fired_at }
}

process on_button(event: ui.button.Pressed) {
  finish { kind: "button", button: event.button }
}

daily_handle = await triggers.register({
  source: cron.Schedule({ expr: "0 8 * * *", tz: "UTC" }),
  target: daily_digest,
  inputs: { tick: trigger.event },
  name: "daily_digest",
  subscription_key: "daily-digest"
})?
button_handle = await triggers.register({
  source: ui.button.pressed({}),
  target: on_button,
  inputs: { event: trigger.event },
  name: "button watcher",
  subscription_key: "button-watcher"
})?
registrations = await triggers.list({ target: daily_digest })?
disabled = await triggers.disable({
  subscription_key: "daily-digest",
  expected_revision: daily_handle.revision
})?
finish {
  daily_handle: daily_handle.id,
  button_handle: button_handle.id,
  registration_count: len(registrations),
  listed_target: registrations[0].target.process_name,
  listed_source: registrations[0].source_type,
  disabled: disabled.enabled
}
"#
        }
        Scenario::SyntaxTextHostEnvironment => {
            r####"
// Exercise parser-heavy string forms, comments, semicolon recovery, and text builtins.
patch = r"""*** Begin Patch
*** Update File: crates/lashlang/src/lib.rs
@@
-old
+new
\n { braces stay raw }
*** End Patch""";
script = r'''python3 - <<'PY'
print("""double quotes are preserved""")
\n { braces stay raw }
PY''';
plain = """first
"quoted"
second""";
"bare expression branch"
pieces = [
  len(patch),
  contains(patch, "*** Begin Patch"),
  starts_with(script, "python3"),
  ends_with(trim(script), "PY"),
  len(split(plain, "\n")),
  slice(plain, 0, 5)
]
finish {
  patch_head: slice(patch, 0, 15),
  script_head: slice(script, 0, 7),
  plain_lines: len(split(plain, "\n")),
  pieces: pieces
}
"####
        }
        Scenario::IntegerRangeHostEnvironment => {
            r#"
items = range(-8, 9)
forward = range(0, 10, 3)
backward = range(7, -3, -2)
stride = ceil_div(len(items), 4)
starts = []
windows = []
for i in range(0, len(items), stride) {
  starts = push(starts, i)
  windows = push(windows, slice(items, i, i + stride))
}
text = "alpha beta gamma beta delta"
first_beta = find(text, "beta")
second_beta = find(text, "beta", first_beta + 1)
finish {
  count: len(items),
  first: items[0],
  last: items[-1],
  forward: forward,
  backward: backward,
  stride: stride,
  starts: starts,
  windows: windows,
  mid: slice(items, 2, -2),
  head: slice(text, null, 5),
  tail: slice(text, -5, null),
  first_beta: first_beta,
  second_beta: second_beta,
  ceil_neg: ceil_div(-10, 3),
  floor_neg: floor_div(-10, 3)
}
"#
        }
        Scenario::FanoutExpressionHostEnvironment => {
            r#"
left = await tools.echo({ value: "left" })
right = await tools.echo({ value: "right" })
computed = len(history) + 39
discarded = ["branch_a", 40 + 2, len(history)]
batched_results = await {
  first: start echo(value: left.value),
  second: start echo(value: right.value)
}
batched = { first: batched_results.first, second: batched_results.second, computed: computed }
finish {
  left: left.value,
  right: right.value,
  computed: computed,
  discarded: discarded,
  first: batched.first?,
  second: batched.second?,
  batched_computed: batched.computed
}
"#
        }
        Scenario::ImageHostEnvironment => {
            r#"
descriptor = to_string(img)
metadata = {
  id: img.id,
  label: img.label,
  size: img.size,
  width: img.width,
  height: img.height,
  missing: img.missing
}
print img
finish {
  metadata: metadata,
  descriptor_has_type: contains(descriptor, "\"type\":\"image\""),
  descriptor_has_id: contains(descriptor, "\"id\":\"img-1\""),
  dims: format("{0}x{1}", img.width, img.height),
  size_bucket: floor_div(img.size, 100)
}
"#
        }
    };
    format!("{BENCH_PROCESS_DECLS}\n{main}")
}
