//! `lash-trace-viewer` renders Lash trace JSONL into a self-contained HTML
//! debugging surface, preserving unknown future events as raw records.
//!
//! All per-event interpretation (kind, title, one-line summary, failure
//! detection, pills) happens once in Rust as a typed match over
//! [`lash_trace::TraceEvent`], producing a [`RenderModel`] of plain serde
//! structs. That model is embedded as JSON and the browser script is a dumb
//! renderer over it — it carries zero event-kind strings and no schema
//! knowledge. Records refused by the typed schema-version fence, or otherwise
//! failing the typed parse, still get a raw-JSON render path so old, future, or
//! unknown events are never dropped.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use lash_trace::{
    TraceContentBlock, TraceError, TraceEvent, TraceLlmRequest, TraceRecord, TraceTokenUsage,
};
use serde::Serialize;
use serde_json::Value;

#[cfg(test)]
mod composition_change_tests;
#[cfg(test)]
mod execution_evidence_tests;
mod language;
mod render;
use language::{language_execution_failed, language_execution_summary, language_execution_title};
use render::interpret_typed;

#[derive(Debug, Parser)]
#[command(
    name = "lash-trace-viewer",
    about = "Render a Lash .trace.jsonl file as a self-contained HTML trace browser"
)]
struct Cli {
    /// Path to a Lash *.trace.jsonl file.
    trace: PathBuf,

    /// Output HTML path. Defaults to <trace-stem>.html beside the trace.
    #[arg(short, long)]
    out: Option<PathBuf>,

    /// Write HTML to stdout instead of a file.
    #[arg(long)]
    stdout: bool,

    /// Page title shown in the viewer.
    #[arg(long)]
    title: Option<String>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("lash-trace-viewer: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let trace = load_trace(&cli.trace)?;
    let title = cli.title.unwrap_or_else(|| {
        cli.trace
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Lash trace")
            .to_string()
    });
    let html = render_html(&title, &cli.trace, &trace)?;

    if cli.stdout {
        io::stdout()
            .write_all(html.as_bytes())
            .context("write HTML to stdout")?;
        return Ok(());
    }

    let out = cli.out.unwrap_or_else(|| default_output_path(&cli.trace));
    fs::write(&out, html).with_context(|| format!("write {}", out.display()))?;
    println!("{}", out.display());
    Ok(())
}

#[derive(Debug)]
struct TraceEntry {
    raw: Value,
    typed: Option<TraceRecord>,
}

#[derive(Debug)]
struct LoadedTrace {
    records: Vec<TraceEntry>,
}

fn load_trace(path: &Path) -> Result<LoadedTrace> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut records = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let raw = serde_json::from_str::<Value>(trimmed)
            .with_context(|| format!("parse trace JSONL line {}", index + 1))?;
        let typed = serde_json::from_value::<TraceRecord>(raw.clone()).ok();
        records.push(TraceEntry { raw, typed });
    }
    if records.is_empty() {
        return Err(anyhow!(
            "trace file contains no records: {}",
            path.display()
        ));
    }
    Ok(LoadedTrace { records })
}

fn default_output_path(trace: &Path) -> PathBuf {
    let mut out = trace.to_path_buf();
    out.set_extension("html");
    out
}

// ---------------------------------------------------------------------------
// Prepared render model — the single source of event interpretation.
// ---------------------------------------------------------------------------

/// Everything the browser script needs to draw the viewer. Built entirely in
/// Rust; the script never re-derives any of these fields from the raw record.
#[derive(Debug, Serialize)]
struct RenderModel {
    events: Vec<RenderEvent>,
    stats: RenderStats,
}

#[derive(Debug, Default, Serialize)]
struct RenderStats {
    total: usize,
    llm_calls: usize,
    failures: usize,
    total_tokens: i64,
}

/// One prepared event row.
#[derive(Debug, Serialize)]
struct RenderEvent {
    /// 1-based position in the trace.
    index: usize,
    kind: String,
    /// Badge colour class: "", "fail", "llm", "tool", or "stream".
    badge: &'static str,
    title: String,
    /// One-line summary; may contain `\n` for the pre-wrapped body.
    summary: String,
    failed: bool,
    /// Belongs on the "LLM Calls" tab.
    is_llm_call: bool,
    /// Belongs on the "Streams" tab.
    is_stream: bool,
    timestamp: String,
    short_time: String,
    turn_index: Option<i64>,
    protocol_iteration: Option<i64>,
    id: String,
    llm_call_id: Option<String>,
    effect_id: Option<String>,
    /// Prebuilt context pills, e.g. "session s1".
    pills: Vec<String>,
    /// The raw record, embedded verbatim for the details/raw views and search.
    raw: Value,
}

fn build_model(trace: &LoadedTrace) -> RenderModel {
    let mut events = Vec::with_capacity(trace.records.len());
    let mut stats = RenderStats {
        total: trace.records.len(),
        ..RenderStats::default()
    };
    for (offset, entry) in trace.records.iter().enumerate() {
        let event = render_event(offset + 1, entry);
        if event.kind == "llm_call_started" {
            stats.llm_calls += 1;
        }
        if event.failed {
            stats.failures += 1;
        }
        stats.total_tokens += token_total(entry);
        events.push(event);
    }
    RenderModel { events, stats }
}

/// Tokens contributed by a record — typed match on the real usage structs.
fn token_total(entry: &TraceEntry) -> i64 {
    let Some(record) = entry.typed.as_ref() else {
        return 0;
    };
    let usage = match &record.event {
        TraceEvent::LlmCallCompleted {
            usage: Some(usage), ..
        }
        | TraceEvent::TokenUsage { usage, .. } => usage,
        _ => return 0,
    };
    usage.input_tokens
        + usage.output_tokens
        + usage.cache_read_input_tokens
        + usage.cache_write_input_tokens
}

fn render_event(index: usize, entry: &TraceEntry) -> RenderEvent {
    let raw = &entry.raw;
    let (kind, title, summary, failed) = match entry.typed.as_ref() {
        Some(record) => {
            let (title, summary, failed) = interpret_typed(&record.event, raw);
            (record.event.kind().to_string(), title, summary, failed)
        }
        None => interpret_raw(raw),
    };

    let badge = if failed || kind.contains("failed") {
        "fail"
    } else if kind.starts_with("llm") {
        "llm"
    } else if kind.starts_with("tool") {
        "tool"
    } else if kind == "language_execution" || kind.ends_with("stream_event") {
        "stream"
    } else {
        ""
    };

    let timestamp = string_field(raw, "timestamp");
    RenderEvent {
        index,
        is_llm_call: kind.starts_with("llm_call_"),
        is_stream: kind.ends_with("stream_event"),
        short_time: short_time(&timestamp),
        turn_index: context_i64(raw, "turn_index"),
        protocol_iteration: context_i64(raw, "protocol_iteration"),
        id: string_field(raw, "id"),
        llm_call_id: context_string(raw, "llm_call_id"),
        effect_id: context_string(raw, "effect_id"),
        pills: pills(raw),
        badge,
        title,
        summary,
        failed,
        kind,
        timestamp,
        raw: raw.clone(),
    }
}

/// Fallback rendering for a record that did not parse into the typed schema
/// (a future or malformed event). Everything comes from the raw JSON.
fn interpret_raw(raw: &Value) -> (String, String, String, bool) {
    let kind = raw
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let failed = kind.contains("failed")
        || raw.get("success") == Some(&Value::Bool(false))
        || raw.get("status").and_then(Value::as_str) == Some("failed");
    (
        kind.clone(),
        kind_title(&kind),
        default_summary(raw),
        failed,
    )
}

fn llm_request_title(request: &TraceLlmRequest) -> String {
    match &request.model_variant {
        Some(variant) => format!("{} / {variant}", request.model),
        None => request.model.clone(),
    }
}

fn summarize_request(request: &TraceLlmRequest) -> String {
    let mut parts = vec![
        format!("{} messages", request.messages.len()),
        format!("{} tools", request.tools.len()),
    ];
    if !request.attachments.is_empty() {
        parts.push(format!("{} attachments", request.attachments.len()));
    }
    let text = request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            TraceContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("{}\n{}", parts.join(", "), truncate_chars(&text, 2000))
}

fn usage_text(usage: &TraceTokenUsage, cumulative: Option<&TraceTokenUsage>) -> String {
    let total = usage.input_tokens
        + usage.output_tokens
        + usage.cache_read_input_tokens
        + usage.cache_write_input_tokens;
    let mut text = format!(
        "tokens {total} = in {}, out {}, cache read {}, cache write {}, reasoning {}",
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_input_tokens,
        usage.cache_write_input_tokens,
        usage.reasoning_output_tokens
    );
    if let Some(cumulative) = cumulative {
        let cumulative_total = cumulative.input_tokens
            + cumulative.output_tokens
            + cumulative.cache_read_input_tokens
            + cumulative.cache_write_input_tokens;
        text.push_str(&format!("\ncumulative {cumulative_total}"));
    }
    text
}

fn failure_detail(error: &TraceError) -> String {
    error
        .raw
        .clone()
        .or_else(|| error.code.clone())
        .unwrap_or_default()
}

fn kind_title(kind: &str) -> String {
    kind.replace('_', " ")
}

fn default_summary(raw: &Value) -> String {
    truncate_chars(&json_compact(raw), 800)
}

fn json_compact<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((byte_idx, _)) => text[..byte_idx].to_string(),
        None => text.to_string(),
    }
}

fn string_field(raw: &Value, key: &str) -> String {
    raw.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn context_string(raw: &Value, key: &str) -> Option<String> {
    raw.get("context")
        .and_then(|context| context.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn context_i64(raw: &Value, key: &str) -> Option<i64> {
    raw.get("context")
        .and_then(|context| context.get(key))
        .and_then(Value::as_i64)
}

fn pills(raw: &Value) -> Vec<String> {
    let mut pills = Vec::new();
    if let Some(session) = context_string(raw, "session_id") {
        pills.push(format!("session {session}"));
    }
    if let Some(turn) = context_string(raw, "turn_id") {
        pills.push(format!("turn {turn}"));
    }
    if let Some(llm) = context_string(raw, "llm_call_id") {
        pills.push(format!("llm {llm}"));
    }
    if let Some(effect) = context_string(raw, "effect_id") {
        pills.push(format!("effect {effect}"));
    }
    if let Some(schema) = raw.get("schema_version").and_then(Value::as_i64) {
        pills.push(format!("schema {schema}"));
    }
    pills
}

fn short_time(timestamp: &str) -> String {
    // Pull HH:MM:SS out of an RFC3339 timestamp, else keep the whole string.
    if let Some(t_index) = timestamp.find('T') {
        let rest = &timestamp[t_index + 1..];
        let time: String = rest.chars().take(8).collect();
        if time.len() == 8 && time.as_bytes()[2] == b':' && time.as_bytes()[5] == b':' {
            return time;
        }
    }
    timestamp.to_string()
}

fn render_html(title: &str, source_path: &Path, trace: &LoadedTrace) -> Result<String> {
    let model = build_model(trace);
    let model_json =
        escape_script_json(&serde_json::to_string(&model).context("serialize render model")?);
    let source = source_path.display().to_string();
    Ok(format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
{css}
</style>
</head>
<body>
<header class="shell-header">
  <div>
    <p class="eyebrow">Lash Trace Viewer</p>
    <h1>{title}</h1>
    <p class="source">{source}</p>
  </div>
  <div class="summary" aria-label="Trace summary">
    <div><strong>{total}</strong><span>events</span></div>
    <div><strong>{llm_calls}</strong><span>LLM calls</span></div>
    <div><strong>{failures}</strong><span>failures</span></div>
    <div><strong>{tokens}</strong><span>tokens</span></div>
  </div>
</header>

<main class="layout">
  <aside class="sidebar">
    <label class="search-label" for="search">Search trace</label>
    <input id="search" class="search" type="search" placeholder="model, tool, text, id">
    <div id="filters" class="filters" aria-label="Event type filters"></div>
    <section class="sidebar-panel">
      <h2>Current Selection</h2>
      <div id="selectionMeta" class="selection-meta">Select an event.</div>
    </section>
  </aside>

  <section class="content">
    <nav class="tabs" aria-label="Trace views">
      <button class="tab active" data-view="timeline">Timeline</button>
      <button class="tab" data-view="llm">LLM Calls</button>
      <button class="tab" data-view="streams">Streams</button>
      <button class="tab" data-view="raw">Raw</button>
    </nav>
    <section id="timeline" class="view active"></section>
    <section id="llm" class="view"></section>
    <section id="streams" class="view"></section>
    <section id="raw" class="view"></section>
  </section>
</main>

<script id="trace-data" type="application/json">{model_json}</script>
<script>
{js}
</script>
</body>
</html>"#,
        title = escape_html(title),
        source = escape_html(&source),
        total = model.stats.total,
        llm_calls = model.stats.llm_calls,
        failures = model.stats.failures,
        tokens = model.stats.total_tokens,
        css = CSS,
        js = JS,
    ))
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn escape_script_json(value: &str) -> String {
    value
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

const CSS: &str = r#"
:root {
  color-scheme: dark;
  --bg: #11110f;
  --panel: #191815;
  --panel-2: #222018;
  --line: #39352b;
  --text: #f2efe6;
  --muted: #aaa28e;
  --dim: #756f61;
  --amber: #e7bd5a;
  --cyan: #81d6d0;
  --red: #ff6f61;
  --green: #93d977;
  --blue: #8fb8ff;
  --shadow: 0 22px 70px rgba(0, 0, 0, .35);
}
* { box-sizing: border-box; }
body {
  margin: 0;
  min-height: 100vh;
  background:
    linear-gradient(90deg, rgba(231, 189, 90, .055) 1px, transparent 1px),
    linear-gradient(180deg, rgba(231, 189, 90, .04) 1px, transparent 1px),
    var(--bg);
  background-size: 28px 28px;
  color: var(--text);
  font: 14px/1.5 ui-monospace, SFMono-Regular, "Cascadia Mono", "Liberation Mono", Menlo, monospace;
}
.shell-header {
  display: flex;
  justify-content: space-between;
  gap: 24px;
  padding: 28px 32px 22px;
  border-bottom: 1px solid var(--line);
  background: rgba(17, 17, 15, .92);
  position: sticky;
  top: 0;
  z-index: 3;
  backdrop-filter: blur(14px);
}
.eyebrow {
  margin: 0 0 5px;
  color: var(--amber);
  text-transform: uppercase;
  letter-spacing: .08em;
  font-size: 12px;
}
h1 {
  margin: 0;
  font: 700 28px/1.15 ui-serif, Georgia, Cambria, serif;
  letter-spacing: 0;
}
.source {
  margin: 7px 0 0;
  color: var(--muted);
  word-break: break-all;
}
.summary {
  display: grid;
  grid-template-columns: repeat(4, minmax(82px, 1fr));
  gap: 10px;
  min-width: 420px;
}
.summary div {
  padding: 12px 14px;
  border: 1px solid var(--line);
  background: var(--panel);
  box-shadow: var(--shadow);
}
.summary strong {
  display: block;
  font-size: 21px;
  line-height: 1;
}
.summary span {
  color: var(--muted);
  font-size: 12px;
}
.layout {
  display: grid;
  grid-template-columns: 320px minmax(0, 1fr);
  min-height: calc(100vh - 117px);
}
.sidebar {
  border-right: 1px solid var(--line);
  padding: 22px;
  position: sticky;
  top: 118px;
  height: calc(100vh - 118px);
  overflow: auto;
  background: rgba(17, 17, 15, .86);
}
.search-label {
  display: block;
  color: var(--muted);
  font-size: 12px;
  margin-bottom: 8px;
}
.search {
  width: 100%;
  min-height: 42px;
  color: var(--text);
  background: #0d0d0b;
  border: 1px solid var(--line);
  border-radius: 4px;
  padding: 10px 12px;
  font: inherit;
}
.filters {
  display: grid;
  gap: 7px;
  margin: 18px 0;
}
.filter {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  min-height: 34px;
  padding: 6px 9px;
  border: 1px solid var(--line);
  background: var(--panel);
  color: var(--text);
  border-radius: 4px;
  cursor: pointer;
  text-align: left;
  font: inherit;
}
.filter.off { opacity: .42; }
.count { color: var(--dim); }
.sidebar-panel {
  border-top: 1px solid var(--line);
  padding-top: 16px;
}
.sidebar-panel h2 {
  margin: 0 0 8px;
  font-size: 13px;
  color: var(--amber);
}
.selection-meta {
  color: var(--muted);
  word-break: break-word;
}
.content {
  min-width: 0;
  padding: 22px 28px 40px;
}
.tabs {
  display: flex;
  gap: 8px;
  margin-bottom: 16px;
  border-bottom: 1px solid var(--line);
}
.tab {
  border: 1px solid var(--line);
  border-bottom: 0;
  background: var(--panel);
  color: var(--muted);
  padding: 9px 13px;
  cursor: pointer;
  font: inherit;
  border-radius: 4px 4px 0 0;
}
.tab.active {
  color: var(--text);
  background: var(--panel-2);
}
.view { display: none; }
.view.active { display: block; }
.event, .llm-card, .stream-card, .raw-card {
  display: grid;
  grid-template-columns: 150px minmax(0, 1fr);
  gap: 18px;
  border: 1px solid var(--line);
  background: rgba(25, 24, 21, .96);
  margin-bottom: 10px;
  box-shadow: var(--shadow);
}
.event.selected, .llm-card.selected, .stream-card.selected {
  outline: 2px solid var(--amber);
}
.event-rail {
  padding: 14px;
  border-right: 1px solid var(--line);
  color: var(--muted);
  background: rgba(0, 0, 0, .14);
}
.event-body {
  min-width: 0;
  padding: 14px 16px 16px 0;
}
.kind {
  display: inline-block;
  color: #11110f;
  background: var(--amber);
  padding: 2px 7px;
  border-radius: 3px;
  font-weight: 700;
}
.kind.fail { background: var(--red); }
.kind.llm { background: var(--cyan); }
.kind.tool { background: var(--green); }
.kind.stream { background: var(--blue); }
.meta-line {
  margin-top: 8px;
  color: var(--dim);
  font-size: 12px;
}
.title-line {
  font-weight: 700;
  margin-bottom: 6px;
}
.summary-line {
  color: var(--muted);
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
details {
  margin-top: 10px;
}
summary {
  cursor: pointer;
  color: var(--amber);
}
pre {
  overflow: auto;
  max-height: 460px;
  margin: 10px 0 0;
  padding: 12px;
  color: #e9e1d0;
  background: #0b0b09;
  border: 1px solid #2b281f;
  border-radius: 4px;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
.pill-row {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin-top: 8px;
}
.pill {
  border: 1px solid var(--line);
  color: var(--muted);
  padding: 2px 7px;
  border-radius: 999px;
  font-size: 12px;
}
.empty {
  padding: 24px;
  border: 1px dashed var(--line);
  color: var(--muted);
}
@media (max-width: 900px) {
  .shell-header { position: static; display: block; }
  .summary { min-width: 0; grid-template-columns: repeat(2, 1fr); margin-top: 16px; }
  .layout { grid-template-columns: 1fr; }
  .sidebar { position: static; height: auto; border-right: 0; border-bottom: 1px solid var(--line); }
  .event, .llm-card, .stream-card, .raw-card { grid-template-columns: 1fr; }
  .event-rail { border-right: 0; border-bottom: 1px solid var(--line); }
  .event-body { padding: 14px; }
}
"#;

// The script is a dumb renderer over the prepared model. It knows the shape of
// a RenderEvent but nothing about event kinds, titles, or summaries — those are
// all computed in Rust.
const JS: &str = r#"
const model = JSON.parse(document.getElementById('trace-data').textContent);
const events = model.events;
const state = { query: '', enabled: new Set(events.map(e => e.kind)), selected: null };

const kinds = [...events.reduce((set, event) => set.add(event.kind), new Set())].sort();
const counts = events.reduce((map, event) => {
  map[event.kind] = (map[event.kind] || 0) + 1;
  return map;
}, {});

const filters = document.getElementById('filters');
for (const kind of kinds) {
  const button = document.createElement('button');
  button.className = 'filter';
  button.dataset.kind = kind;
  button.innerHTML = `<span>${escapeHtml(kind)}</span><span class="count">${counts[kind]}</span>`;
  button.addEventListener('click', () => {
    if (state.enabled.has(kind)) state.enabled.delete(kind);
    else state.enabled.add(kind);
    button.classList.toggle('off', !state.enabled.has(kind));
    renderAll();
  });
  filters.appendChild(button);
}

document.getElementById('search').addEventListener('input', event => {
  state.query = event.target.value.trim().toLowerCase();
  renderAll();
});

document.querySelectorAll('.tab').forEach(tab => {
  tab.addEventListener('click', () => {
    document.querySelectorAll('.tab').forEach(t => t.classList.remove('active'));
    document.querySelectorAll('.view').forEach(v => v.classList.remove('active'));
    tab.classList.add('active');
    document.getElementById(tab.dataset.view).classList.add('active');
  });
});

function searchText(event) {
  return (JSON.stringify(event.raw) + '\n' + event.title + '\n' + event.summary).toLowerCase();
}

function filteredEvents() {
  return events.filter(event => {
    if (!state.enabled.has(event.kind)) return false;
    if (!state.query) return true;
    return searchText(event).includes(state.query);
  });
}

function renderAll() {
  renderList('timeline', filteredEvents(), 'event', 'No events match the current filters.');
  renderList('llm', filteredEvents().filter(e => e.is_llm_call), 'llm-card', 'No LLM call events match.');
  renderList('streams', filteredEvents().filter(e => e.is_stream), 'stream-card',
    'No stream events match. Run with --trace-level extended to capture provider/runtime stream records.');
  renderRaw();
}

function renderList(viewId, visible, className, emptyText) {
  const root = document.getElementById(viewId);
  root.innerHTML = visible.length ? '' : `<div class="empty">${escapeHtml(emptyText)}</div>`;
  visible.forEach(event => root.appendChild(eventCard(event, className)));
}

function renderRaw() {
  const root = document.getElementById('raw');
  const visible = filteredEvents();
  root.innerHTML = visible.length ? '' : `<div class="empty">No raw records match.</div>`;
  visible.forEach(event => {
    const card = document.createElement('article');
    card.className = 'raw-card';
    card.innerHTML = `<div class="event-rail"><span class="kind">${escapeHtml(event.kind)}</span><div class="meta-line">#${event.index}</div></div><div class="event-body"><pre>${escapeHtml(rawJson(event))}</pre></div>`;
    root.appendChild(card);
  });
}

function eventCard(event, className) {
  const card = document.createElement('article');
  card.className = className;
  card.dataset.id = event.id;
  card.addEventListener('click', () => selectEvent(event, card));
  const turn = event.turn_index ?? 'na';
  const step = event.protocol_iteration ?? 'na';
  card.innerHTML = `
    <div class="event-rail">
      <span class="kind ${event.badge}">${escapeHtml(event.kind)}</span>
      <div class="meta-line">${escapeHtml(event.short_time)}</div>
      <div class="meta-line">turn ${escapeHtml(String(turn))} · step ${escapeHtml(String(step))}</div>
    </div>
    <div class="event-body">
      <div class="title-line">${escapeHtml(event.title)}</div>
      <div class="summary-line">${escapeHtml(event.summary)}</div>
      ${pillRow(event.pills)}
      <details><summary>Raw JSON</summary><pre>${escapeHtml(rawJson(event))}</pre></details>
    </div>`;
  return card;
}

function selectEvent(event, element) {
  document.querySelectorAll('.selected').forEach(el => el.classList.remove('selected'));
  element.classList.add('selected');
  state.selected = event.id;
  document.getElementById('selectionMeta').innerHTML = `
    <div><strong>${escapeHtml(event.kind)}</strong></div>
    <div>${escapeHtml(event.timestamp)}</div>
    <div>${escapeHtml(event.id)}</div>
    <div>llm: ${escapeHtml(event.llm_call_id || 'none')}</div>
    <div>effect: ${escapeHtml(event.effect_id || 'none')}</div>`;
}

function pillRow(pills) {
  if (!pills || !pills.length) return '';
  return `<div class="pill-row">${pills.map(p => `<span class="pill">${escapeHtml(p)}</span>`).join('')}</div>`;
}

function rawJson(event) {
  return JSON.stringify(event.raw, null, 2);
}

function escapeHtml(value) {
  return String(value ?? '')
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&#39;');
}

renderAll();
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use lash_trace::{
        TraceAgentFrameSwitch, TraceContentBlock, TraceContext, TraceError, TraceEvent,
        TraceLanguageExecutionEvent, TraceLanguageExecutionIdentity, TraceLanguageExecutionMap,
        TraceLlmMessage, TraceLlmRequest, TraceLlmResponse, TraceProviderRequestEvent,
        TraceProviderStreamEvent, TraceRecord, TraceRuntimeScope, TraceRuntimeStreamEvent,
        TraceRuntimeSubject, TraceTokenUsage, TraceToolCallOutcome, TraceToolCallOutput,
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    pub(super) fn loaded_trace(records: Vec<TraceRecord>) -> LoadedTrace {
        LoadedTrace {
            records: records
                .into_iter()
                .map(|record| TraceEntry {
                    raw: serde_json::to_value(&record).expect("trace record JSON"),
                    typed: Some(record),
                })
                .collect(),
        }
    }

    fn temp_trace_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "lash-trace-viewer-{name}-{}-{nanos}.jsonl",
            std::process::id()
        ))
    }

    fn identity() -> TraceLanguageExecutionIdentity {
        TraceLanguageExecutionIdentity {
            scope: TraceRuntimeScope::new("s1"),
            subject: TraceRuntimeSubject::Process {
                process_id: "p1".to_string(),
            },
            module_ref: "module".to_string(),
            entry_kind: "process".to_string(),
            entry_ref: None,
            entry_name: "main".to_string(),
        }
    }

    fn sample_request() -> TraceLlmRequest {
        TraceLlmRequest {
            model: "gpt-5.5".to_string(),
            model_variant: Some("high".to_string()),
            messages: vec![TraceLlmMessage {
                role: "system".to_string(),
                blocks: vec![TraceContentBlock::Text {
                    text: "hello".to_string(),
                    cache_breakpoint: false,
                }],
            }],
            attachments: Vec::new(),
            tools: Vec::new(),
            tool_choice: "auto".to_string(),
            output_spec: None,
            stream: true,
        }
    }

    fn usage() -> TraceTokenUsage {
        TraceTokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_input_tokens: 2,
            cache_write_input_tokens: 1,
            reasoning_output_tokens: 3,
        }
    }

    /// One sample of every `TraceEvent` variant. The list must stay complete —
    /// the drift-guard test below asserts each renders a non-empty kind/title,
    /// and `interpret_typed`'s exhaustive match refuses to compile if a variant
    /// is added without a rendering.
    pub(super) fn every_variant() -> Vec<TraceEvent> {
        vec![
            TraceEvent::SessionStarted {
                metadata: Default::default(),
            },
            TraceEvent::TurnStarted {
                metadata: Default::default(),
            },
            TraceEvent::PromptBuilt {
                prompt_hash: "h".to_string(),
                prompt_chars: 42,
                components: Vec::new(),
            },
            composition_change_tests::sample(),
            TraceEvent::RollingHistoryCompactionNeeded {
                context_budget_tokens: 30_000,
                max_context_tokens: 40_000,
                threshold_tokens: 20_000,
            },
            TraceEvent::RollingHistoryPromptPruned {
                context_budget_tokens: 30_000,
                max_context_tokens: 40_000,
                dropped_prefix_messages: 2,
                retained_messages: 1,
            },
            TraceEvent::RollingHistoryCompactionStarted {
                source_messages: 3,
                instructions_present: true,
            },
            TraceEvent::RollingHistoryCompactionCompleted { summary_nodes: 1 },
            TraceEvent::LlmCallStarted {
                request: sample_request(),
            },
            TraceEvent::LlmCallCompleted {
                response: TraceLlmResponse {
                    text: "ok".to_string(),
                    duration_ms: 12,
                    terminal_reason: None,
                    parts: None,
                    generation_disposition: None,
                },
                usage: Some(usage()),
                provider_usage: None,
                stream_summary: None,
                attempts: Some(vec![
                    lash_trace::TraceRetryAttempt {
                        ordinal: 1,
                        outcome: "failed".to_string(),
                        duration_ms: 12,
                        reason: Some("http_429".to_string()),
                        delay_ms: Some(250),
                        execution_evidence: Some(lash_trace::TraceExecutionEvidence {
                            served_model: Some("served-model".to_string()),
                            provider_response_id: Some("provider-response-1".to_string()),
                            reasoning_output_tokens: Some(0),
                            provider_finish_reason: Some("stop".to_string()),
                            collection_interruption: Some("protocol_abort".to_string()),
                            ..Default::default()
                        }),
                    },
                    lash_trace::TraceRetryAttempt {
                        ordinal: 2,
                        outcome: "completed".to_string(),
                        duration_ms: 8,
                        reason: None,
                        delay_ms: None,
                        execution_evidence: Some(lash_trace::TraceExecutionEvidence {
                            served_model: Some("served-model-2".to_string()),
                            ..Default::default()
                        }),
                    },
                ]),
            },
            TraceEvent::LlmCallFailed {
                error: TraceError {
                    message: "boom".to_string(),
                    retryable: false,
                    terminal_reason: None,
                    code: Some("bad".to_string()),
                    raw: None,
                },
                stream_summary: None,
                attempts: None,
            },
            TraceEvent::ProviderRequest {
                event: TraceProviderRequestEvent {
                    provider: "openai_compatible".to_string(),
                    sequence: 0,
                    elapsed_ms: 1,
                    endpoint: "responses".to_string(),
                    body_len: 13,
                    body_sha256: "sha".to_string(),
                    body_json: Some(serde_json::json!({"model": "m"})),
                    body_json_omitted_reason: None,
                },
            },
            TraceEvent::ProviderStreamEvent {
                event: TraceProviderStreamEvent {
                    provider: "anthropic".to_string(),
                    sequence: 1,
                    elapsed_ms: 2,
                    event_name: "delta".to_string(),
                    item_id: None,
                    output_index: None,
                    raw_len: 3,
                    raw_sha256: "sha".to_string(),
                    raw_json: None,
                },
            },
            TraceEvent::RuntimeStreamEvent {
                event: TraceRuntimeStreamEvent {
                    sequence: 1,
                    elapsed_ms: 2,
                    event_name: "delta".to_string(),
                    raw_text: Some("raw".to_string()),
                    visible_text: Some("visible".to_string()),
                    item_id: None,
                    output_index: None,
                    call_id: None,
                    tool_name: None,
                    input_json: None,
                    usage: None,
                },
            },
            TraceEvent::ToolCallStarted {
                call_id: Some("c1".to_string()),
                name: "read_file".to_string(),
                args: serde_json::json!({"path": "x"}),
            },
            TraceEvent::ToolCallCompleted {
                call_id: Some("c1".to_string()),
                name: "read_file".to_string(),
                args: serde_json::json!({"path": "x"}),
                output: TraceToolCallOutput {
                    outcome: TraceToolCallOutcome::Success(serde_json::json!({"ok": true})),
                    control: None,
                },
                duration_ms: 4,
                attempts: Some(vec![
                    lash_trace::TraceRetryAttempt {
                        ordinal: 1,
                        outcome: "failed".to_string(),
                        duration_ms: 1,
                        reason: Some("busy".to_string()),
                        delay_ms: Some(10),
                        execution_evidence: None,
                    },
                    lash_trace::TraceRetryAttempt {
                        ordinal: 2,
                        outcome: "completed".to_string(),
                        duration_ms: 3,
                        reason: None,
                        delay_ms: None,
                        execution_evidence: None,
                    },
                ]),
            },
            TraceEvent::JournaledEffectStarted {
                effect_name: "lash:turn:llm:1".to_string(),
                effect_kind: "llm_call".to_string(),
            },
            TraceEvent::JournaledEffectSettled {
                effect_name: "lash:turn:llm:1".to_string(),
                effect_kind: "llm_call".to_string(),
                status: "completed".to_string(),
            },
            TraceEvent::DurableWaitParked {
                wait_kind: "await_event".to_string(),
            },
            TraceEvent::DurableWaitResolved {
                wait_kind: "await_event".to_string(),
                resolution: "ok".to_string(),
            },
            TraceEvent::DurableTimerStarted { duration_ms: 250 },
            TraceEvent::DurableTimerResolved {
                duration_ms: 250,
                status: "resolved".to_string(),
            },
            TraceEvent::DurableSegmentBoundary {
                reason: "journal_budget".to_string(),
                effects_executed: 10_000,
                journaled_bytes_estimate: None,
            },
            TraceEvent::StoreErrorObserved {
                operation: "session_restore".to_string(),
                error_class: "StoredDataCorrupt".to_string(),
                message: "bad session head".to_string(),
            },
            TraceEvent::ProtocolStep {
                plugin_id: "rlm".to_string(),
                payload: serde_json::json!({"tool_calls": []}),
            },
            TraceEvent::TokenUsage {
                usage: usage(),
                cumulative: Some(usage()),
            },
            TraceEvent::LanguageExecution {
                language: "lashlang".to_string(),
                event: TraceLanguageExecutionEvent::ExecutionStarted {
                    event_key: "k".to_string(),
                    identity: identity(),
                    execution_map: TraceLanguageExecutionMap::default(),
                },
            },
            TraceEvent::TurnCompleted {
                status: "completed".to_string(),
                done_reason: "modelstop".to_string(),
                agent_frame_switch: Some(TraceAgentFrameSwitch {
                    frame_key: "frame-key/v1/example".to_string(),
                }),
            },
            TraceEvent::Custom {
                name: "x".to_string(),
                payload: serde_json::json!({"ok": true}),
            },
        ]
    }

    #[test]
    fn default_output_path_replaces_extension() {
        assert_eq!(
            default_output_path(Path::new("session.trace.jsonl")),
            PathBuf::from("session.trace.html")
        );
    }

    #[test]
    fn render_contains_embedded_trace_data() {
        let trace = loaded_trace(vec![TraceRecord::new(
            TraceContext::default().for_session("s1"),
            TraceEvent::TurnStarted {
                metadata: Default::default(),
            },
        )]);

        let html = render_html("title", Path::new("trace.jsonl"), &trace).expect("render");

        assert!(html.contains("Lash Trace Viewer"));
        assert!(html.contains("trace-data"));
        assert!(html.contains("turn_started"));
    }

    #[test]
    fn restate_hosted_turn_keeps_session_turn_and_durable_step_parity() {
        let context = TraceContext::default()
            .for_session("restate-session")
            .for_turn("restate-turn");
        let trace = loaded_trace(vec![
            TraceRecord::new(
                context.clone(),
                TraceEvent::TurnStarted {
                    metadata: Default::default(),
                },
            ),
            TraceRecord::new(
                context.clone(),
                TraceEvent::JournaledEffectStarted {
                    effect_name: "lash:llm:1".to_string(),
                    effect_kind: "llm_call".to_string(),
                },
            ),
            TraceRecord::new(
                context,
                TraceEvent::JournaledEffectSettled {
                    effect_name: "lash:llm:1".to_string(),
                    effect_kind: "llm_call".to_string(),
                    status: "completed".to_string(),
                },
            ),
        ]);
        let model = build_model(&trace);

        assert_eq!(
            model
                .events
                .iter()
                .map(|event| event.kind.as_str())
                .collect::<Vec<_>>(),
            vec![
                "turn_started",
                "journaled_effect_started",
                "journaled_effect_settled"
            ]
        );
        assert!(model.events.iter().all(|event| {
            event
                .pills
                .iter()
                .any(|pill| pill == "session restate-session")
        }));
        assert!(
            model
                .events
                .iter()
                .all(|event| event.pills.iter().any(|pill| pill == "turn restate-turn"))
        );
    }

    #[test]
    fn render_escapes_script_breakout_sequences() {
        let trace = loaded_trace(vec![TraceRecord::new(
            TraceContext::default(),
            TraceEvent::Custom {
                name: "x".to_string(),
                payload: serde_json::json!({"text": "</script><script>alert(1)</script>"}),
            },
        )]);

        let html = render_html("title", Path::new("trace.jsonl"), &trace).expect("render");

        assert!(!html.contains("</script><script>alert"));
        assert!(html.contains("\\u003c/script\\u003e"));
    }

    #[test]
    fn every_trace_event_variant_renders_kind_and_title() {
        for event in every_variant() {
            let record = TraceRecord::new(TraceContext::default().for_session("s1"), event);
            let entry = TraceEntry {
                raw: serde_json::to_value(&record).expect("record json"),
                typed: Some(record),
            };
            let rendered = render_event(1, &entry);
            assert!(!rendered.kind.is_empty(), "empty kind");
            assert!(
                !rendered.title.is_empty(),
                "kind {} rendered an empty title",
                rendered.kind
            );
        }
    }

    #[test]
    fn stats_count_llm_calls_failures_and_tokens_from_typed_events() {
        let trace = loaded_trace(every_variant().into_iter().fold(
            Vec::new(),
            |mut records, event| {
                records.push(TraceRecord::new(
                    TraceContext::default().for_session("s1"),
                    event,
                ));
                records
            },
        ));
        let model = build_model(&trace);
        assert_eq!(model.stats.llm_calls, 1);
        // The failed LLM call and typed store-integrity evidence are failures.
        assert_eq!(model.stats.failures, 2);
        // LlmCallCompleted usage (18) + TokenUsage usage (18); reasoning excluded.
        assert_eq!(model.stats.total_tokens, 36);
    }

    #[test]
    fn tool_failure_is_flagged_and_badged() {
        let record = TraceRecord::new(
            TraceContext::default(),
            TraceEvent::ToolCallCompleted {
                call_id: Some("c1".to_string()),
                name: "read_file".to_string(),
                args: serde_json::json!({"path": "missing"}),
                output: TraceToolCallOutput {
                    outcome: TraceToolCallOutcome::Failure(serde_json::json!({"message": "no"})),
                    control: None,
                },
                duration_ms: 2,
                attempts: None,
            },
        );
        let entry = TraceEntry {
            raw: serde_json::to_value(&record).expect("json"),
            typed: Some(record),
        };
        let rendered = render_event(1, &entry);
        assert!(rendered.failed);
        assert_eq!(rendered.badge, "fail");
        assert!(rendered.summary.starts_with("error in 2 ms"));
    }

    #[test]
    fn protocol_step_payload_renders_generically() {
        let record = TraceRecord::new(
            TraceContext::default(),
            TraceEvent::ProtocolStep {
                plugin_id: "rlm".to_string(),
                payload: serde_json::json!({"tool_calls": [{"name": "shell"}]}),
            },
        );
        let entry = TraceEntry {
            raw: serde_json::to_value(&record).expect("json"),
            typed: Some(record),
        };
        let rendered = render_event(1, &entry);
        // A future field (tool_calls) shows up with no viewer change.
        assert!(rendered.summary.contains("tool_calls"));
        assert!(rendered.summary.contains("shell"));
    }

    #[test]
    fn load_trace_keeps_old_and_unknown_valid_json_records_raw() {
        let path = temp_trace_path("raw-fallback");
        std::fs::write(
            &path,
            concat!(
                r#"{"schema_version":999,"type":"future_event","payload":{"text":"kept raw"}}"#,
                "\n",
                r#"{"schema_version":3,"id":"legacy-record","timestamp":"2026-05-11T11:42:01.234+00:00","context":{"session_id":"legacy-session"},"type":"session_started"}"#,
            ),
        )
        .expect("write trace");

        let trace = load_trace(&path).expect("load trace");
        std::fs::remove_file(&path).ok();

        assert_eq!(trace.records.len(), 2);
        assert!(trace.records.iter().all(|record| record.typed.is_none()));

        // The unknown record still renders through the raw-JSON fallback path.
        let rendered = render_event(1, &trace.records[0]);
        assert_eq!(rendered.kind, "future_event");
        assert_eq!(rendered.title, "future event");
        assert!(rendered.summary.contains("kept raw"));

        let html = render_html("title", &path, &trace).expect("render");
        assert!(html.contains("future_event"));
        assert!(html.contains("kept raw"));

        let rendered = render_event(2, &trace.records[1]);
        assert_eq!(rendered.kind, "session_started");
        assert_eq!(rendered.title, "session started");
        assert_eq!(rendered.id, "legacy-record");
        assert_eq!(rendered.pills, vec!["session legacy-session", "schema 3"]);
    }

    #[test]
    fn load_trace_fails_on_invalid_json() {
        let path = temp_trace_path("invalid");
        std::fs::write(&path, "{\"type\":\"turn_started\"").expect("write trace");

        let err = load_trace(&path).expect_err("invalid JSON should fail");
        std::fs::remove_file(&path).ok();

        assert!(format!("{err:#}").contains("parse trace JSONL line 1"));
    }

    #[test]
    fn load_trace_fails_on_empty_files() {
        let path = temp_trace_path("empty");
        std::fs::write(&path, "\n  \n").expect("write trace");

        let err = load_trace(&path).expect_err("empty trace should fail");
        std::fs::remove_file(&path).ok();

        assert!(format!("{err:#}").contains("trace file contains no records"));
    }
}
