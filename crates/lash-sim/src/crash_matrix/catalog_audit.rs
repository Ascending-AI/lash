//! No catalog name survives in what a definition world persisted (FIG-4179).
//!
//! A process definition is an immutable value named by its content-derived
//! id (ADR 0113 §3.6); names, revisions and lifecycles are the host's own
//! catalog, which lash never stores. A symbol search cannot show that the
//! bytes a run wrote agree, so this check reads them: every table and column
//! declaration, every cell of every table the world's stores hold (SQLite
//! files, a PostgreSQL database, or a live world's in-memory SQLite), and the
//! stored bytes of every journal entry of every invocation the engine holds.
//!
//! A value fails the audit when
//!
//! - its bytes spell a retired registry vocabulary word (the named registry's
//!   tables, its registration command, its revision referrer kind), or
//! - a JSON document it holds, at any depth and inside JSON-in-a-string or
//!   JSON embedded in a journal entry's protobuf, has an object that carries
//!   a definition (a definition id, an engine kind, or the stock engine's
//!   `module_ref`/`host_requirements_ref`/`process_ref` descriptor) beside a
//!   catalog field: a name, a revision, an owner scope, a lifecycle, a
//!   fingerprint or a change sequence.
//!
//! Source export names and run labels are diagnostic data, not catalog
//! fields: a module's export list and a run's `label` pass. The check also
//! refuses to pass vacuously: across its reads of one world it must have
//! found at least one definition-bearing document (a world whose dead start
//! admitted nothing may end with its definition reclaimed and nothing left
//! to read, so a start cell also reads it before the crash), and on the
//! double at least one journal entry.
//!
//! A cell runs it as an [`Expected::audits`](super::invariants::Expected)
//! entry, once the world's invariants held: it reads every store and
//! journal, which on a live server would spend the detection bound.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::engine::{Engine, StoredCell};
use super::invariants::CustomCheck;
use super::world::CrashWorld;

/// Vocabulary only the retired named registry wrote.
const RETIRED_VOCABULARY: &[&str] = &[
    "process_definitions",
    "process_definition_hosts",
    "register_definition",
    "RegisterDefinition",
    "register_process_definition",
    "RegisterProcessDefinition",
    "definition_revision",
    "DefinitionRevision",
];

/// Keys that mark an object as carrying a definition.
const DEFINITION_KEYS: &[&str] = &["definition_id", "$lash_definition_id", "engine_kind"];

/// The stock engine's canonical descriptor: an object with all three carries
/// a definition.
const DESCRIPTOR_KEYS: &[&str] = &["module_ref", "host_requirements_ref", "process_ref"];

/// Catalog fields no definition-bearing object may carry.
const CATALOG_KEYS: &[&str] = &[
    "name",
    "process_name",
    "definition_name",
    "revision",
    "expected_revision",
    "owner_scope",
    "lifecycle",
    "fingerprint",
    "change_seq",
];

/// What one audit read.
#[derive(Debug, Default)]
struct Audited {
    cells: usize,
    documents: usize,
    definition_bearing: usize,
    journal_entries: usize,
    violations: Vec<String>,
}

/// One world's audit: every read's content violations, and what all its
/// reads have seen, which the final check judges for vacuity.
#[derive(Clone, Default)]
pub(crate) struct CatalogAudit {
    definitions_seen: Arc<AtomicUsize>,
    journal_entries_seen: Arc<AtomicUsize>,
}

impl CatalogAudit {
    /// The final check: a read, then whether this audit's reads, the ones
    /// before it included, found anything to judge.
    pub(crate) fn check(&self) -> CustomCheck {
        let audit = self.clone();
        Arc::new(move |world: &CrashWorld| {
            let audit = audit.clone();
            Box::pin(async move {
                let mut violations = audit.read(world).await;
                if audit.definitions_seen.load(Ordering::SeqCst) == 0 {
                    violations.push(
                        "no store or journal read held a definition-bearing document".to_owned(),
                    );
                }
                if matches!(world.engine(), Engine::Double(_))
                    && audit.journal_entries_seen.load(Ordering::SeqCst) == 0
                {
                    violations.push("the engine held no journal entry to audit".to_owned());
                }
                violations
            })
        })
    }

    /// Read every store cell and journal entry `world` holds now: the
    /// violations they show.
    pub(crate) async fn read(&self, world: &CrashWorld) -> Vec<String> {
        let audited = audit(world).await;
        self.definitions_seen
            .fetch_add(audited.definition_bearing, Ordering::SeqCst);
        self.journal_entries_seen
            .fetch_add(audited.journal_entries, Ordering::SeqCst);
        audited.violations
    }
}

/// The check for a world audited only once it recovered.
pub(crate) fn no_catalog_name() -> CustomCheck {
    CatalogAudit::default().check()
}

async fn audit(world: &CrashWorld) -> Audited {
    let engine = world.engine();
    let mut audited = Audited::default();
    match engine.stored_cells().await {
        Ok(cells) => {
            for cell in cells {
                audit_cell(&cell, &mut audited);
            }
        }
        Err(error) => audited
            .violations
            .push(format!("the stores are unreadable: {error}")),
    }
    let stored_definitions = audited.definition_bearing;
    for invocation in world.invocations().await {
        match engine.journal_entries(&invocation.id).await {
            Ok(entries) => {
                for (entry, bytes) in entries {
                    audited.journal_entries += 1;
                    audit_cell(
                        &StoredCell {
                            location: format!(
                                "journal {} ({}) entry {entry}",
                                invocation.id, invocation.target
                            ),
                            documents: embedded_documents(&bytes),
                            bytes,
                        },
                        &mut audited,
                    );
                }
            }
            Err(error) => audited.violations.push(format!(
                "the journal of {} ({}) is unreadable: {error}",
                invocation.id, invocation.target
            )),
        }
    }
    println!(
        "catalog audit on the {} engine: {} stored cell(s), {} journal entr(ies), {} document(s), \
         {} definition-bearing ({} stored), {} violation(s)",
        engine.name(),
        audited.cells,
        audited.journal_entries,
        audited.documents,
        audited.definition_bearing,
        stored_definitions,
        audited.violations.len()
    );
    audited
}

fn audit_cell(cell: &StoredCell, audited: &mut Audited) {
    audited.cells += 1;
    let text = String::from_utf8_lossy(&cell.bytes);
    for word in RETIRED_VOCABULARY {
        if text.contains(word) {
            audited.violations.push(format!(
                "{} spells the retired registry word `{word}`",
                cell.location
            ));
        }
    }
    for document in &cell.documents {
        audited.documents += 1;
        walk(document, &cell.location, "$", audited);
    }
}

fn walk(value: &serde_json::Value, location: &str, path: &str, audited: &mut Audited) {
    match value {
        serde_json::Value::Object(fields) => {
            let carries_definition = DEFINITION_KEYS.iter().any(|key| fields.contains_key(*key))
                || DESCRIPTOR_KEYS.iter().all(|key| fields.contains_key(*key));
            if carries_definition {
                audited.definition_bearing += 1;
                for key in CATALOG_KEYS {
                    if fields.contains_key(*key) {
                        audited.violations.push(format!(
                            "{location} at {path}: a definition-bearing object carries the \
                             catalog field `{key}`"
                        ));
                    }
                }
            }
            for (key, field) in fields {
                walk(field, location, &format!("{path}.{key}"), audited);
            }
        }
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                walk(item, location, &format!("{path}[{index}]"), audited);
            }
        }
        serde_json::Value::String(text) => {
            for document in embedded_documents(text.as_bytes()) {
                walk(&document, location, &format!("{path}<json>"), audited);
            }
        }
        _ => {}
    }
}

/// Every JSON object embedded in `bytes`: a journal entry's protobuf holds
/// its values as raw JSON bytes, and a stored string may hold a JSON
/// document of its own.
fn embedded_documents(bytes: &[u8]) -> Vec<serde_json::Value> {
    let mut documents = Vec::new();
    let mut at = 0;
    while let Some(offset) = bytes[at..].iter().position(|byte| *byte == b'{') {
        let start = at + offset;
        let mut stream =
            serde_json::Deserializer::from_slice(&bytes[start..]).into_iter::<serde_json::Value>();
        match stream.next() {
            Some(Ok(document)) if document.is_object() => {
                documents.push(document);
                at = start + stream.byte_offset().max(1);
            }
            _ => at = start + 1,
        }
    }
    documents
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audited(document: serde_json::Value) -> Audited {
        let mut audited = Audited::default();
        audit_cell(
            &StoredCell {
                location: "fixture".to_owned(),
                bytes: document.to_string().into_bytes(),
                documents: vec![document],
            },
            &mut audited,
        );
        audited
    }

    #[test]
    fn a_named_definition_fails_and_an_unnamed_one_passes() {
        let named = audited(serde_json::json!({
            "identity": {"kind": "lashlang", "definition_id": "lash.definition:sha256:00", "name": "scan"},
        }));
        assert_eq!(named.definition_bearing, 1);
        assert_eq!(named.violations.len(), 1, "{:?}", named.violations);

        let labelled = audited(serde_json::json!({
            "identity": {"kind": "lashlang", "definition_id": "lash.definition:sha256:00", "label": "scan"},
        }));
        assert_eq!(labelled.definition_bearing, 1);
        assert!(labelled.violations.is_empty(), "{:?}", labelled.violations);
    }

    #[test]
    fn a_descriptor_carrying_its_export_name_fails_inside_a_json_string() {
        let descriptor = serde_json::json!({
            "module_ref": "m", "host_requirements_ref": "h", "process_ref": "p", "process_name": "main",
        });
        let wrapped = audited(serde_json::json!({ "payload": descriptor.to_string() }));
        assert_eq!(wrapped.definition_bearing, 1);
        assert_eq!(wrapped.violations.len(), 1, "{:?}", wrapped.violations);
    }

    #[test]
    fn an_export_list_is_source_data_and_passes() {
        let exports = audited(serde_json::json!({
            "module_ref": "m", "processes": [{"name": "main", "process_ref": "p"}],
        }));
        assert_eq!(exports.definition_bearing, 0);
        assert!(exports.violations.is_empty(), "{:?}", exports.violations);
    }

    #[test]
    fn retired_registry_vocabulary_fails_wherever_it_is_spelled() {
        let command = audited(serde_json::json!({"op": "register_definition"}));
        assert_eq!(command.violations.len(), 1, "{:?}", command.violations);
    }

    #[test]
    fn json_embedded_in_protobuf_bytes_is_found() {
        let mut bytes = vec![0x0a, 0x12, b'{'];
        bytes.extend_from_slice(br#"{"definition_id":"d","revision":3}"#);
        bytes.extend_from_slice(&[0x10, 0x01]);
        let documents = embedded_documents(&bytes);
        assert_eq!(documents.len(), 1);
        let mut audited = Audited::default();
        audit_cell(
            &StoredCell {
                location: "journal".to_owned(),
                bytes,
                documents,
            },
            &mut audited,
        );
        assert_eq!(audited.violations.len(), 1, "{:?}", audited.violations);
    }
}
