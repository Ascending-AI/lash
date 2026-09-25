//! The foreign-key half of the shared constraint registry: expected keys,
//! their canonical renderings, the live-inspection comparison, and the DDL
//! clause parser the congruence gate and the SQLite inspector share.
//!
//! Split from `required_constraints` to keep that module under the file-size
//! budget; everything here is re-exported through it.

use std::collections::BTreeMap;

use super::{
    SqliteConstraintDatabase, Token, TokenKind, lex_sqlite_ddl, sqlite_create_table_body_opening,
};
use crate::StoreError;

/// One foreign-key clause Lash requires in the published store schemas: the
/// same logical key rendered once per backend, so a row added here is gated
/// on both stores at once.
///
/// Keys are pinned by what they enforce — referencing columns, referenced
/// table and columns, referential actions, and deferral — rather than by
/// constraint name, because most of Lash's published foreign keys are
/// declared anonymously and a name match could not see them anyway.
/// `sqlite_databases` lists every SQLite component carrying the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExpectedForeignKey {
    /// Which SQLite schema components carry the table declaring the key.
    pub sqlite_databases: &'static [SqliteConstraintDatabase],
    /// The key as SQLite declares it, when a counterpart exists.
    pub sqlite: Option<RenderedForeignKey>,
    /// The key as PostgreSQL declares it, when a counterpart exists.
    pub postgres: Option<RenderedForeignKey>,
}

/// A `FOREIGN KEY`/`REFERENCES` clause as one backend declares it. Column
/// order follows the declaration; `on_delete`/`on_update` use the canonical
/// lower-case spellings (`no action`, `restrict`, `cascade`, `set null`,
/// `set default`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RenderedForeignKey {
    /// Table declaring the key (the child side).
    pub table: &'static str,
    /// Referencing columns, in declaration order.
    pub columns: &'static [&'static str],
    /// Referenced table.
    pub referenced_table: &'static str,
    /// Referenced columns, in declaration order.
    pub referenced_columns: &'static [&'static str],
    /// `ON DELETE` action.
    pub on_delete: &'static str,
    /// `ON UPDATE` action.
    pub on_update: &'static str,
    /// Whether enforcement may defer to commit.
    pub deferrable: bool,
    /// Whether a deferrable key starts each transaction deferred.
    pub initially_deferred: bool,
}

const fn rendered_foreign_key(
    table: &'static str,
    columns: &'static [&'static str],
    referenced_table: &'static str,
    referenced_columns: &'static [&'static str],
    on_delete: &'static str,
    deferrable: bool,
    initially_deferred: bool,
) -> RenderedForeignKey {
    RenderedForeignKey {
        table,
        columns,
        referenced_table,
        referenced_columns,
        on_delete,
        on_update: "no action",
        deferrable,
        initially_deferred,
    }
}

const fn expected_foreign_key(
    sqlite_databases: &'static [SqliteConstraintDatabase],
    sqlite: RenderedForeignKey,
    postgres: RenderedForeignKey,
) -> ExpectedForeignKey {
    ExpectedForeignKey {
        sqlite_databases,
        sqlite: Some(sqlite),
        postgres: Some(postgres),
    }
}

/// A key only SQLite's effect journal declares: PostgreSQL is storage only and
/// holds no effect journal (ADR 0104).
const fn sqlite_only_foreign_key(
    sqlite_databases: &'static [SqliteConstraintDatabase],
    sqlite: RenderedForeignKey,
) -> ExpectedForeignKey {
    ExpectedForeignKey {
        sqlite_databases,
        sqlite: Some(sqlite),
        postgres: None,
    }
}

const fn postgres_only_foreign_key(postgres: RenderedForeignKey) -> ExpectedForeignKey {
    ExpectedForeignKey {
        sqlite_databases: &[],
        sqlite: None,
        postgres: Some(postgres),
    }
}

/// The foreign keys Lash's published schemas must declare, one row per key
/// with each backend's rendering beside the other. The congruence gate also
/// rejects any `FOREIGN KEY`/`REFERENCES` clause in the published DDL that is
/// not registered here, so a new key cannot land unpinned.
pub const EXPECTED_FOREIGN_KEYS: &[ExpectedForeignKey] = &[
    expected_foreign_key(
        &[SqliteConstraintDatabase::DurableCore],
        rendered_foreign_key(
            "checkpoint_blob_refs",
            &["checkpoint_ref"],
            "blobs",
            &["hash"],
            "cascade",
            false,
            false,
        ),
        rendered_foreign_key(
            "lash_checkpoint_blob_refs",
            &["checkpoint_ref"],
            "lash_blobs",
            &["hash"],
            "cascade",
            false,
            false,
        ),
    ),
    // `blob_ref` keeps a PostgreSQL-only key: SQLite's table declares no
    // reference on the column and leans on `idx_checkpoint_blob_refs_blob_ref`
    // plus owner-scoped delete ordering instead.
    postgres_only_foreign_key(rendered_foreign_key(
        "lash_checkpoint_blob_refs",
        &["blob_ref"],
        "lash_blobs",
        &["hash"],
        "no action",
        false,
        false,
    )),
    expected_foreign_key(
        &[SqliteConstraintDatabase::DurableCore],
        rendered_foreign_key(
            "session_meta_pending_observer_intents",
            &["session_id"],
            "session_meta",
            &["session_id"],
            "cascade",
            false,
            false,
        ),
        rendered_foreign_key(
            "lash_session_meta_pending_observer_intents",
            &["session_id"],
            "lash_session_meta",
            &["session_id"],
            "cascade",
            false,
            false,
        ),
    ),
    expected_foreign_key(
        &[SqliteConstraintDatabase::DurableCore],
        rendered_foreign_key(
            "queued_work_items",
            &["batch_id"],
            "queued_work_batches",
            &["batch_id"],
            "cascade",
            false,
            false,
        ),
        rendered_foreign_key(
            "lash_queued_work_items",
            &["batch_id"],
            "lash_queued_work_batches",
            &["batch_id"],
            "cascade",
            false,
            false,
        ),
    ),
    expected_foreign_key(
        &[SqliteConstraintDatabase::DurableCore],
        rendered_foreign_key(
            "queued_run_members",
            &["session_id", "scope_id"],
            "queued_runs",
            &["session_id", "scope_id"],
            "no action",
            false,
            false,
        ),
        rendered_foreign_key(
            "lash_queued_run_members",
            &["session_id", "scope_id"],
            "lash_queued_runs",
            &["session_id", "scope_id"],
            "no action",
            false,
            false,
        ),
    ),
    expected_foreign_key(
        &[SqliteConstraintDatabase::DurableCore],
        rendered_foreign_key(
            "artifact_owners",
            &["namespace", "artifact_ref"],
            "artifact_refs",
            &["namespace", "artifact_ref"],
            "cascade",
            false,
            false,
        ),
        rendered_foreign_key(
            "lash_artifact_owners",
            &["namespace", "artifact_ref"],
            "lash_lashlang_artifacts",
            &["namespace", "artifact_ref"],
            "cascade",
            false,
            false,
        ),
    ),
    expected_foreign_key(
        &[SqliteConstraintDatabase::ProcessRegistry],
        rendered_foreign_key(
            "process_events",
            &["process_id", "process_incarnation"],
            "processes",
            &["process_id", "incarnation"],
            "cascade",
            false,
            false,
        ),
        rendered_foreign_key(
            "lash_process_events",
            &["process_id", "process_incarnation"],
            "lash_processes",
            &["process_id", "incarnation"],
            "cascade",
            false,
            false,
        ),
    ),
    expected_foreign_key(
        &[SqliteConstraintDatabase::ProcessRegistry],
        rendered_foreign_key(
            "process_wake_deliveries",
            &["process_id", "process_incarnation"],
            "processes",
            &["process_id", "incarnation"],
            "cascade",
            false,
            false,
        ),
        rendered_foreign_key(
            "lash_process_wake_deliveries",
            &["process_id", "process_incarnation"],
            "lash_processes",
            &["process_id", "incarnation"],
            "cascade",
            false,
            false,
        ),
    ),
    expected_foreign_key(
        &[SqliteConstraintDatabase::ProcessRegistry],
        rendered_foreign_key(
            "process_observers",
            &["process_id", "process_incarnation"],
            "processes",
            &["process_id", "incarnation"],
            "cascade",
            false,
            false,
        ),
        rendered_foreign_key(
            "lash_process_observers",
            &["process_id", "process_incarnation"],
            "lash_processes",
            &["process_id", "incarnation"],
            "cascade",
            false,
            false,
        ),
    ),
    expected_foreign_key(
        &[SqliteConstraintDatabase::ProcessRegistry],
        rendered_foreign_key(
            "process_artifact_cleanup",
            &["process_id", "incarnation"],
            "process_tombstones",
            &["process_id", "incarnation"],
            "restrict",
            false,
            false,
        ),
        rendered_foreign_key(
            "lash_process_artifact_cleanup",
            &["process_id", "incarnation"],
            "lash_process_tombstones",
            &["process_id", "incarnation"],
            "restrict",
            false,
            false,
        ),
    ),
    expected_foreign_key(
        &[SqliteConstraintDatabase::ProcessRegistry],
        rendered_foreign_key(
            "process_leases",
            &["process_id"],
            "processes",
            &["process_id"],
            "cascade",
            false,
            false,
        ),
        rendered_foreign_key(
            "lash_process_leases",
            &["process_id"],
            "lash_processes",
            &["process_id"],
            "cascade",
            false,
            false,
        ),
    ),
    expected_foreign_key(
        &[SqliteConstraintDatabase::ProcessRegistry],
        rendered_foreign_key(
            "process_segment_handovers",
            &["process_id"],
            "processes",
            &["process_id"],
            "cascade",
            false,
            false,
        ),
        rendered_foreign_key(
            "lash_process_segment_handovers",
            &["process_id"],
            "lash_processes",
            &["process_id"],
            "cascade",
            false,
            false,
        ),
    ),
    expected_foreign_key(
        &[SqliteConstraintDatabase::Triggers],
        rendered_foreign_key(
            "trigger_deliveries",
            &["occurrence_id"],
            "trigger_occurrences",
            &["occurrence_id"],
            "cascade",
            false,
            false,
        ),
        rendered_foreign_key(
            "lash_trigger_deliveries",
            &["occurrence_id"],
            "lash_trigger_occurrences",
            &["occurrence_id"],
            "cascade",
            false,
            false,
        ),
    ),
    // PostgreSQL-only child table: SQLite keeps the cancel record's affected
    // inputs inside `record_json` instead of a structural table.
    postgres_only_foreign_key(rendered_foreign_key(
        "lash_turn_cancel_affected_inputs",
        &["session_id", "turn_id"],
        "lash_turn_cancel_requests",
        &["session_id", "turn_id"],
        "cascade",
        false,
        false,
    )),
    sqlite_only_foreign_key(
        &[SqliteConstraintDatabase::EffectReplay],
        rendered_foreign_key(
            "runtime_effect_replay",
            &["group_key"],
            "runtime_effect_group",
            &["group_key"],
            "no action",
            true,
            true,
        ),
    ),
    sqlite_only_foreign_key(
        &[SqliteConstraintDatabase::EffectReplay],
        rendered_foreign_key(
            "runtime_effect_group_child",
            &["group_key"],
            "runtime_effect_group",
            &["group_key"],
            "no action",
            true,
            true,
        ),
    ),
];

/// One live foreign-key clause read by a store adapter. Fields carry the same
/// canonical spellings [`RenderedForeignKey`] documents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectedForeignKey {
    pub table: String,
    pub columns: Vec<String>,
    pub referenced_table: String,
    pub referenced_columns: Vec<String>,
    pub on_delete: String,
    pub on_update: String,
    pub deferrable: bool,
    pub initially_deferred: bool,
    pub validated: bool,
    pub enforced: bool,
}

/// One registered foreign key that failed live inspection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequiredForeignKeyFinding {
    /// A registered key was absent from the inspected catalog.
    Missing { table: String, clause: String },
    /// A key with the registered identity was present but carried different
    /// referential actions or deferral semantics.
    Altered {
        table: String,
        clause: String,
        expected: String,
        actual: String,
    },
    /// A declared key matched no registered identity.
    Unexpected { table: String, clause: String },
    /// PostgreSQL kept the key `NOT VALID`.
    Unvalidated { table: String, clause: String },
    /// PostgreSQL declared the key `NOT ENFORCED`.
    Unenforced { table: String, clause: String },
}

fn foreign_key_clause_text(
    columns: &[String],
    referenced_table: &str,
    referenced_columns: &[String],
) -> String {
    format!(
        "({}) references {} ({})",
        columns.join(", "),
        referenced_table,
        referenced_columns.join(", ")
    )
}

fn rendered_foreign_key_identity(
    key: &RenderedForeignKey,
) -> (String, String, BTreeMap<String, String>) {
    (
        key.table.to_string(),
        key.referenced_table.to_string(),
        key.columns
            .iter()
            .map(|column| (*column).to_string())
            .zip(
                key.referenced_columns
                    .iter()
                    .map(|column| (*column).to_string()),
            )
            .collect(),
    )
}

fn inspected_foreign_key_identity(
    key: &InspectedForeignKey,
) -> (String, String, BTreeMap<String, String>) {
    (
        key.table.clone(),
        key.referenced_table.clone(),
        key.columns
            .iter()
            .cloned()
            .zip(key.referenced_columns.iter().cloned())
            .collect(),
    )
}

fn rendered_foreign_key_clause(key: &RenderedForeignKey) -> String {
    foreign_key_clause_text(
        &key.columns
            .iter()
            .map(|column| (*column).to_string())
            .collect::<Vec<_>>(),
        key.referenced_table,
        &key.referenced_columns
            .iter()
            .map(|column| (*column).to_string())
            .collect::<Vec<_>>(),
    )
}

fn inspected_foreign_key_clause(key: &InspectedForeignKey) -> String {
    foreign_key_clause_text(&key.columns, &key.referenced_table, &key.referenced_columns)
}

/// Compares the live foreign keys a store adapter read against the
/// registered set for one backend.
pub fn compare_required_foreign_keys(
    backend: &'static str,
    expected: &[RenderedForeignKey],
    actual: Vec<InspectedForeignKey>,
) -> Result<Vec<RequiredForeignKeyFinding>, StoreError> {
    let mut actual_by_identity = BTreeMap::new();
    for key in actual {
        let identity = inspected_foreign_key_identity(&key);
        if actual_by_identity.insert(identity, key).is_some() {
            return Err(StoreError::RequiredConstraintInspectionInconclusive {
                backend,
                table: "<catalog>".to_string(),
                constraint: "<duplicate key>".to_string(),
                detail: "the catalog returned a duplicate foreign-key identity".to_string(),
            });
        }
    }

    let mut findings = Vec::new();
    for expected in expected {
        let identity = rendered_foreign_key_identity(expected);
        let clause = rendered_foreign_key_clause(expected);
        let Some(actual) = actual_by_identity.remove(&identity) else {
            findings.push(RequiredForeignKeyFinding::Missing {
                table: expected.table.to_string(),
                clause,
            });
            continue;
        };
        if actual.on_delete != expected.on_delete
            || actual.on_update != expected.on_update
            || actual.deferrable != expected.deferrable
            || actual.initially_deferred != expected.initially_deferred
        {
            findings.push(RequiredForeignKeyFinding::Altered {
                table: expected.table.to_string(),
                clause: clause.clone(),
                expected: format!(
                    "on delete {}, on update {}, deferrable={}, initially deferred={}",
                    expected.on_delete,
                    expected.on_update,
                    expected.deferrable,
                    expected.initially_deferred
                ),
                actual: format!(
                    "on delete {}, on update {}, deferrable={}, initially deferred={}",
                    actual.on_delete,
                    actual.on_update,
                    actual.deferrable,
                    actual.initially_deferred
                ),
            });
        }
        if !actual.validated {
            findings.push(RequiredForeignKeyFinding::Unvalidated {
                table: expected.table.to_string(),
                clause: clause.clone(),
            });
        }
        if !actual.enforced {
            findings.push(RequiredForeignKeyFinding::Unenforced {
                table: expected.table.to_string(),
                clause,
            });
        }
    }
    for (_, unexpected) in actual_by_identity {
        let clause = inspected_foreign_key_clause(&unexpected);
        findings.push(RequiredForeignKeyFinding::Unexpected {
            table: unexpected.table,
            clause,
        });
    }
    Ok(findings)
}

/// One foreign-key clause parsed out of a `CREATE TABLE` body, in the same
/// canonical spelling [`RenderedForeignKey`] documents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedForeignKeyClause {
    pub columns: Vec<String>,
    pub referenced_table: String,
    pub referenced_columns: Vec<String>,
    pub on_delete: String,
    pub on_update: String,
    pub deferrable: bool,
    pub initially_deferred: bool,
}

/// Extract every `FOREIGN KEY`/`REFERENCES` clause — table-level or
/// column-level — from one `CREATE TABLE` statement, so both the congruence
/// gate and live SQLite inspection can pin the same canonical clauses.
pub fn extract_foreign_key_clauses(source: &str) -> Result<Vec<ParsedForeignKeyClause>, String> {
    let tokens = lex_sqlite_ddl(source)?;
    let opening = sqlite_create_table_body_opening(&tokens)?;
    let mut clauses = Vec::new();
    let mut item_start = opening + 1;
    let mut depth = 1_usize;
    for index in opening + 1..tokens.len() {
        match tokens[index].kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => {
                depth -= 1;
                if depth == 0 {
                    if let Some(clause) = foreign_key_in_table_item(&tokens[item_start..index])? {
                        clauses.push(clause);
                    }
                    return Ok(clauses);
                }
            }
            TokenKind::Comma if depth == 1 => {
                if let Some(clause) = foreign_key_in_table_item(&tokens[item_start..index])? {
                    clauses.push(clause);
                }
                item_start = index + 1;
            }
            _ => {}
        }
    }
    Err("CREATE TABLE statement has no closing `)`".to_string())
}

fn foreign_key_in_table_item(tokens: &[Token]) -> Result<Option<ParsedForeignKeyClause>, String> {
    let mut index = 0_usize;
    if tokens
        .first()
        .is_some_and(|token| token.is_ident("constraint"))
    {
        if tokens.get(1).and_then(Token::identifier).is_none() {
            return Ok(None);
        }
        index = 2;
    }
    if tokens
        .get(index)
        .is_some_and(|token| token.is_ident("foreign"))
    {
        if !tokens
            .get(index + 1)
            .is_some_and(|token| token.is_ident("key"))
            || tokens
                .get(index + 2)
                .is_none_or(|token| token.kind != TokenKind::LParen)
        {
            return Ok(None);
        }
        let (columns, next) = parse_identifier_list(tokens, index + 2)?;
        let mut clause = parse_references_tail(tokens, next)?;
        clause.columns = columns;
        return Ok(Some(clause));
    }
    if index != 0 {
        return Ok(None);
    }
    let Some(column) = tokens.first().and_then(Token::identifier) else {
        return Ok(None);
    };
    let mut depth = 0_usize;
    let mut references = None;
    for (offset, token) in tokens.iter().enumerate() {
        match token.kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => depth = depth.saturating_sub(1),
            _ if depth == 0 && token.is_ident("references") => {
                references = Some(offset);
                break;
            }
            _ => {}
        }
    }
    let Some(references) = references else {
        return Ok(None);
    };
    let mut clause = parse_references_tail(tokens, references)?;
    clause.columns = vec![column.to_string()];
    Ok(Some(clause))
}

/// Parses `identifier (, identifier)* )` starting at the `(` token.
fn parse_identifier_list(tokens: &[Token], open: usize) -> Result<(Vec<String>, usize), String> {
    let mut columns = Vec::new();
    let mut index = open + 1;
    loop {
        let Some(name) = tokens.get(index).and_then(Token::identifier) else {
            return Err("foreign-key column list has no identifier".to_string());
        };
        columns.push(name.to_string());
        index += 1;
        match tokens.get(index).map(|token| &token.kind) {
            Some(TokenKind::Comma) => index += 1,
            Some(TokenKind::RParen) => return Ok((columns, index + 1)),
            _ => return Err("foreign-key column list has no closing `)`".to_string()),
        }
    }
}

/// Parses `REFERENCES <table> [(columns)]` plus any `ON`/`DEFERRABLE`/
/// `INITIALLY` tail, starting at the `references` token.
fn parse_references_tail(
    tokens: &[Token],
    references: usize,
) -> Result<ParsedForeignKeyClause, String> {
    let mut index = references + 1;
    let Some(referenced_table) = tokens.get(index).and_then(Token::identifier) else {
        return Err("`REFERENCES` is missing its table name".to_string());
    };
    index += 1;
    let mut referenced_columns = Vec::new();
    if tokens
        .get(index)
        .is_some_and(|token| token.kind == TokenKind::LParen)
    {
        let parsed = parse_identifier_list(tokens, index)?;
        referenced_columns = parsed.0;
        index = parsed.1;
    }
    let mut clause = ParsedForeignKeyClause {
        columns: Vec::new(),
        referenced_table: referenced_table.to_string(),
        referenced_columns,
        on_delete: "no action".to_string(),
        on_update: "no action".to_string(),
        deferrable: false,
        initially_deferred: false,
    };
    while index < tokens.len() {
        if tokens[index].is_ident("on") {
            let Some(action) = tokens.get(index + 1).and_then(Token::identifier) else {
                return Err("`ON` in a foreign key is missing its event".to_string());
            };
            let action_index = index + 2;
            let parsed = parse_referential_action(tokens, action_index)?;
            match action {
                "delete" => clause.on_delete = parsed.0,
                "update" => clause.on_update = parsed.0,
                _ => {
                    return Err(format!(
                        "unrecognized foreign-key referential event `{action}`"
                    ));
                }
            }
            index = parsed.1;
        } else if tokens[index].is_ident("not") {
            if !tokens
                .get(index + 1)
                .is_some_and(|token| token.is_ident("deferrable"))
            {
                return Err("unrecognized `NOT` in a foreign-key tail".to_string());
            }
            clause.deferrable = false;
            index += 2;
        } else if tokens[index].is_ident("deferrable") {
            clause.deferrable = true;
            index += 1;
        } else if tokens[index].is_ident("initially") {
            if tokens
                .get(index + 1)
                .is_some_and(|token| token.is_ident("deferred"))
            {
                clause.deferrable = true;
                clause.initially_deferred = true;
            } else if tokens
                .get(index + 1)
                .is_some_and(|token| token.is_ident("immediate"))
            {
                clause.initially_deferred = false;
            } else {
                return Err("unrecognized `INITIALLY` in a foreign-key tail".to_string());
            }
            index += 2;
        } else if tokens[index].is_ident("match") {
            // `MATCH SIMPLE|FULL|PARTIAL` changes pairing semantics we do not
            // use; reject so a future declaration lands loudly.
            return Err("`MATCH` in a foreign-key tail is not supported".to_string());
        } else {
            return Err(format!(
                "unrecognized token `{:?}` in a foreign-key tail",
                tokens[index].kind
            ));
        }
    }
    Ok(clause)
}

/// Parses `CASCADE|RESTRICT|NO ACTION|SET NULL|SET DEFAULT`, returning the
/// canonical spelling and the next index.
fn parse_referential_action(tokens: &[Token], index: usize) -> Result<(String, usize), String> {
    let Some(word) = tokens.get(index).and_then(Token::identifier) else {
        return Err("referential action is missing".to_string());
    };
    match word {
        "cascade" => Ok(("cascade".to_string(), index + 1)),
        "restrict" => Ok(("restrict".to_string(), index + 1)),
        "no" if tokens
            .get(index + 1)
            .is_some_and(|token| token.is_ident("action")) =>
        {
            Ok(("no action".to_string(), index + 2))
        }
        "set"
            if tokens
                .get(index + 1)
                .is_some_and(|token| token.is_ident("null")) =>
        {
            Ok(("set null".to_string(), index + 2))
        }
        "set"
            if tokens
                .get(index + 1)
                .is_some_and(|token| token.is_ident("default")) =>
        {
            Ok(("set default".to_string(), index + 2))
        }
        _ => Err(format!("unrecognized referential action `{word}`")),
    }
}
