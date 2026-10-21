//! FIG-6256: counted document and definition identity over the sp-docrepr
//! spike corpus: the kernel conformance documents, every definition of the
//! built registry, lowered TypeScript cells and synthetic workflows.
//! Run an optimized build with the checkout root as the only argument; it
//! needs `perf_event_open` for its own user-space instructions.
#![expect(unsafe_code, reason = "Linux instructions:u counter")]

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use lash_kernel_doc::Document;
use lash_vocabulary::legacy_names::EffectName;
use lash_vocabulary::legacy_names::Name;
use lash_vocabulary::types::Param;
use lash_vocabulary::types::Signature;
use lash_vocabulary::types::Type;

#[repr(C)]
#[derive(Default)]
struct PerfEventAttr {
    kind: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
}

struct Counter(i32);

impl Counter {
    fn open() -> Result<Self> {
        let attr = PerfEventAttr {
            size: 64,
            config: 1,                      // PERF_COUNT_HW_INSTRUCTIONS
            flags: 1 | (1 << 5) | (1 << 6), // disabled, exclude kernel/hypervisor
            ..Default::default()
        };
        let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, &attr, 0, -1, -1, 0u64) };
        ensure!(
            fd >= 0,
            "perf_event_open: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(i32::try_from(fd)?))
    }

    /// Instructions per call of `operation`, over `iterations` calls.
    fn per_call<T>(&self, iterations: u32, mut operation: impl FnMut() -> T) -> Result<u64> {
        unsafe {
            ensure!(libc::ioctl(self.0, 0x2403, 0) == 0, "reset counter");
            ensure!(libc::ioctl(self.0, 0x2400, 0) == 0, "enable counter");
        }
        for _ in 0..iterations {
            std::hint::black_box(operation());
        }
        let mut instructions = 0u64;
        unsafe {
            ensure!(libc::ioctl(self.0, 0x2401, 0) == 0, "disable counter");
            ensure!(
                libc::read(self.0, (&raw mut instructions).cast(), 8) == 8,
                "read counter"
            );
        }
        Ok(instructions / u64::from(iterations))
    }
}

impl Drop for Counter {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

fn documents_in(json: &serde_json::Value, out: &mut Vec<String>) {
    match json {
        serde_json::Value::Object(members) => {
            for (key, value) in members {
                match value {
                    serde_json::Value::String(text) if key == "document" => out.push(text.clone()),
                    _ => documents_in(value, out),
                }
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|item| documents_in(item, out)),
        _ => {}
    }
}

fn conformance(root: &std::path::Path) -> Result<Vec<(String, Document)>> {
    let mut seen = BTreeSet::new();
    let mut found = Vec::new();
    let mut directories = vec![root.join("crates/lash-kernel-conformance/corpus")];
    while let Some(directory) = directories.pop() {
        let mut entries = std::fs::read_dir(&directory)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort();
        for path in entries {
            if path.is_dir() {
                directories.push(path);
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
            let mut texts = Vec::new();
            documents_in(&json, &mut texts);
            let rule = path
                .file_stem()
                .context("a corpus file has a name")?
                .to_string_lossy()
                .into_owned();
            for text in texts {
                if seen.insert(text.clone())
                    && let Ok(document) = lash_kernel_doc::parse_document(&text)
                {
                    found.push((rule.clone(), document));
                }
            }
        }
    }
    Ok(found)
}

fn lower(
    embedding: &lash_vm_worker::Embedding,
    source: &str,
    bindings: &[&str],
) -> Option<Document> {
    let mut effects = BTreeMap::new();
    let mut roots = BTreeSet::new();
    let bytes = source.as_bytes();
    // The spike gives each named tool its Any -> Any signature.
    for root in ["tools", "control", "processes"] {
        let needle = format!("{root}.");
        let mut at = 0;
        while let Some(found) = source[at..].find(&needle) {
            let start = at + found + needle.len();
            let preceded = at + found > 0
                && (bytes[at + found - 1].is_ascii_alphanumeric() || bytes[at + found - 1] == b'_');
            let end = source[start..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .map_or(source.len(), |end| start + end);
            if !preceded && end > start {
                effects.insert(
                    EffectName::new(format!("{root}.{}", &source[start..end])).ok()?,
                    Signature {
                        params: vec![Param {
                            name: Name::new("x"),
                            ty: Type::Any,
                            optional: false,
                        }],
                        result: Type::Any,
                    },
                );
                roots.insert(Name::new(root));
            }
            at = end.max(start);
        }
    }
    let controls = BTreeMap::from([(
        EffectName::new("control.finish").ok()?,
        BTreeSet::from([lash_kernel_dialect::EffectControl::Finish]),
    )]);
    let bindings: BTreeSet<Name> = bindings.iter().map(|name| Name::new(*name)).collect();
    let functions = BTreeMap::new();
    let environment_namespaces = lash_kernel_dialect::Namespaces::of_effects(
        &effects,
        &lash_vocabulary::operations::ContractId::new("1"),
        &controls,
    )
    .ok()?;
    let environment =
        lash_kernel_dialect::Environment::new(embedding.library(), &environment_namespaces);
    let cell = lash_kernel_dialect::CellEnvironment {
        bindings: &bindings,
        functions: &functions,
    };
    embedding
        .lower(
            "typescript",
            source,
            &environment,
            lash_kernel_dialect::Mode::Cell(cell),
        )?
        .ok()
        .map(|lowered| lowered.document.into_document())
}

const AGENT_CELLS: &[&str] = &[
    r#"const hits = await tools.search({ query: "lash kernel encoding", limit: 20 });
const pages = await Promise.all(hits.results.slice(0, 8).map((hit) => tools.fetch({ url: hit.url })));
const notes = [];
for (const page of pages) {
  if (page.status !== 200) { continue; }
  const words = page.text.split(" ").filter((w) => w.length > 3);
  notes.push({ url: page.url, words: words.length, title: page.title ?? "untitled" });
}
notes.sort((a, b) => b.words - a.words);
const summary = notes.map((n, i) => `${i + 1}. ${n.title} (${n.words} words) ${n.url}`).join("\n");
await control.finish({ count: notes.length, summary });"#,
    r#"let attempts = 0;
let result = null;
while (attempts < 5 && result === null) {
  attempts = attempts + 1;
  try {
    const reply = await tools.deploy({ service: "api", version: "1.2.3", attempt: attempts });
    if (reply.ok) { result = reply; } else { await tools.sleepMs({ ms: 100 * attempts }); }
  } catch (error) {
    await tools.log({ level: "warn", message: `attempt ${attempts} failed: ${error}` });
  }
}
const report = { attempts, ok: result !== null, id: result === null ? null : result.id };
await control.finish(report);"#,
    r#"const rows = await tools.query({ sql: "select * from orders where day = $1", params: ["2026-10-14"] });
const byCustomer = {};
for (const row of rows.rows) {
  const key = row.customer;
  if (!(key in byCustomer)) { byCustomer[key] = { total: 0, count: 0, items: [] }; }
  byCustomer[key].total = byCustomer[key].total + row.amount;
  byCustomer[key].count = byCustomer[key].count + 1;
  byCustomer[key].items.push(row.sku);
}
const top = Object.entries(byCustomer).sort((a, b) => b[1].total - a[1].total).slice(0, 10);
const lines = top.map(([name, v]) => `${name}: ${v.count} orders, ${v.total.toFixed(2)} total, ${[...new Set(v.items)].length} skus`);
await tools.writeFile({ path: "report.txt", text: lines.join("\n") });
await control.finish({ customers: top.length, lines });"#,
];

fn workflow(steps: usize) -> String {
    let mut source = String::new();
    for step in 0..steps {
        source.push_str(&format!(
            r#"async function step{step}(input) {{
  const started = {{ step: {step}, input, notes: [] }};
  for (let i = 0; i < input.items.length; i = i + 1) {{
    const item = input.items[i];
    try {{
      const reply = await tools.processItem({{ step: {step}, index: i, item, label: `step {step} item ${{i}}` }});
      if (reply.ok && reply.score > {step}) {{
        started.notes.push({{ index: i, score: reply.score, text: reply.text.slice(0, 64) }});
      }} else {{
        await tools.log({{ level: "info", message: `step {step} skipped ${{i}}` }});
      }}
    }} catch (error) {{
      started.notes.push({{ index: i, error: `${{error}}` }});
    }}
  }}
  return started;
}}
"#
        ));
    }
    source.push_str("let state = { items: [1, 2, 3] };\nconst results = [];\n");
    for step in 0..steps {
        source.push_str(&format!("results.push(await step{step}(state));\n"));
    }
    source.push_str("await control.finish({ results });\n");
    source
}

/// Enough calls that a small document's count is not one call's noise.
fn iterations(json_bytes: usize) -> u32 {
    u32::try_from((4_000_000 / json_bytes.max(1)).clamp(3, 2_000)).unwrap_or(3)
}

fn main() -> Result<()> {
    let root = std::path::PathBuf::from(std::env::args().nth(1).context("checkout root")?);
    let counter = Counter::open()?;
    let embedding = lash_vm_worker::built_embedding(&lash_vm_client::WorkerTuning::default())?;
    let mut documents = conformance(&root)?
        .into_iter()
        .map(|(rule, document)| ("conformance", rule, document))
        .collect::<Vec<_>>();
    let replay: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(
        root.join("crates/lash-perf/workloads/replay/ts-codemode.json"),
    )?)?;
    for (index, turn) in replay["turns"]
        .as_array()
        .context("replay turns")?
        .iter()
        .enumerate()
    {
        for reply in turn["replies"].as_array().context("replay replies")? {
            let Some(lines) = reply["cell"].as_array() else {
                continue;
            };
            let source = lines
                .iter()
                .filter_map(|line| line.as_str())
                .map(|line| line.replace("{turn}", "7"))
                .collect::<Vec<_>>()
                .join("\n");
            let bindings: &[&str] = if index == 0 {
                &[]
            } else {
                &["big", "describe", "last"]
            };
            if let Some(document) = lower(&embedding, &source, bindings) {
                documents.push(("rlm_cell", format!("ts-codemode-{index}"), document));
            }
        }
    }
    let workload =
        lash_perf::workload::Workload::parse(include_str!("../../workloads/figments-v1.json"))?;
    let generator = lash_perf::workload::Generator::new(&workload, "docrepr")?;
    for ordinal in 0..40 {
        let plan = generator.plan(ordinal % 7, ordinal)?;
        if let Some(source) = generator.provider_response(&plan, 1)?.cell_source
            && let Some(document) = lower(&embedding, &source, &[])
        {
            documents.push(("rlm_cell", format!("figments-{ordinal}"), document));
        }
    }
    for (index, source) in AGENT_CELLS.iter().enumerate() {
        if let Some(document) = lower(&embedding, source, &[]) {
            documents.push(("rlm_cell", format!("agent-{index}"), document));
        }
    }
    for steps in [10, 40, 95] {
        if let Some(document) = lower(&embedding, &workflow(steps), &[]) {
            documents.push(("workflow", format!("workflow-{steps}"), document));
        }
    }
    let mut rows = Vec::new();
    for (corpus, name, document) in &documents {
        let json_bytes = document.to_json()?.len();
        let instructions = counter.per_call(iterations(json_bytes), || document.identity())?;
        // The canonical token encoding alone: the rest is the digest.
        let encode = counter.per_call(iterations(json_bytes), || document.canonical_bytes())?;
        rows.push(serde_json::json!({
            "corpus": corpus, "name": name, "json_bytes": json_bytes,
            "token_bytes": document.canonical_bytes().len(),
            "identity": document.identity().to_string(), "instructions": instructions,
            "encode_instructions": encode,
        }));
    }
    let mut release_instructions = 0u64;
    let registry = embedding.registry()?;
    for (id, registered) in registry.iter() {
        let definition = registered.definition();
        let json_bytes = definition.to_json()?.len();
        let instructions = counter.per_call(iterations(json_bytes), || definition.identity())?;
        release_instructions += instructions;
        rows.push(serde_json::json!({
            "corpus": "release", "name": definition.name.as_str(), "json_bytes": json_bytes,
            "identity": id.to_string(), "instructions": instructions,
        }));
    }
    println!(
        "{}",
        serde_json::json!({
            "rows": rows,
            "release_definitions": registry.len(),
            "release_instructions": release_instructions,
        })
    );
    Ok(())
}
