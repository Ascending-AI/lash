//! Shared expected definitions and comparison support for named SQL `CHECK`s.
//!
//! Store adapters use this module for explicit read-only inspection. The
//! source-congruence gate imports the same registries, so the expected
//! expressions have one owner.

use std::collections::BTreeMap;

use crate::StoreError;

/// One named `CHECK` Lash requires in a published store schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExpectedConstraint {
    pub sqlite_database: Option<SqliteConstraintDatabase>,
    pub table: &'static str,
    pub name: &'static str,
    pub expression: &'static str,
}

/// SQLite schema component that owns a registered named `CHECK`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SqliteConstraintDatabase {
    DurableCore,
    ProcessRegistry,
    Triggers,
    EffectReplay,
}

const fn expected_constraint(
    table: &'static str,
    name: &'static str,
    expression: &'static str,
) -> ExpectedConstraint {
    ExpectedConstraint {
        sqlite_database: None,
        table,
        name,
        expression,
    }
}

const fn sqlite_constraint(
    database: SqliteConstraintDatabase,
    table: &'static str,
    name: &'static str,
    expression: &'static str,
) -> ExpectedConstraint {
    ExpectedConstraint {
        sqlite_database: Some(database),
        table,
        name,
        expression,
    }
}

/// Named `CHECK`s required from SQLite's four published schema components.
pub const SQLITE_EXPECTED_CONSTRAINTS: &[ExpectedConstraint] = &[
    sqlite_constraint(
        SqliteConstraintDatabase::DurableCore,
        "pending_turn_inputs",
        "ck_pending_turn_inputs_state",
        "state IN ('pending_active', 'deferred_next_turn', 'accepted', 'cancelled', 'completed')",
    ),
    sqlite_constraint(
        SqliteConstraintDatabase::DurableCore,
        "pending_turn_inputs",
        "ck_pending_turn_inputs_state_ingress",
        "(json_extract(ingress_json, '$.scope') = 'active_turn' AND state IN ('pending_active', 'accepted', 'cancelled', 'completed')) OR (json_extract(ingress_json, '$.scope') = 'next_turn' AND state IN ('deferred_next_turn', 'cancelled', 'completed'))",
    ),
    sqlite_constraint(
        SqliteConstraintDatabase::DurableCore,
        "pending_turn_inputs",
        "ck_pending_turn_inputs_claim_id_token_all_or_none",
        "(claim_id IS NULL AND claim_token IS NULL) OR (claim_id IS NOT NULL AND claim_token IS NOT NULL)",
    ),
    sqlite_constraint(
        SqliteConstraintDatabase::DurableCore,
        "queued_work_batches",
        "ck_queued_work_batches_work_kind",
        "work_kind IN ('turn', 'control')",
    ),
    sqlite_constraint(
        SqliteConstraintDatabase::DurableCore,
        "queued_work_batches",
        "ck_queued_work_batches_delivery_policy",
        "delivery_policy IN ('earliest_safe_boundary', 'after_current_turn_commit')",
    ),
    sqlite_constraint(
        SqliteConstraintDatabase::DurableCore,
        "queued_work_batches",
        "ck_queued_work_batches_claim_id_token_all_or_none",
        "(claim_id IS NULL AND claim_token IS NULL) OR (claim_id IS NOT NULL AND claim_token IS NOT NULL)",
    ),
    sqlite_constraint(
        SqliteConstraintDatabase::DurableCore,
        "session_execution_leases",
        "ck_session_execution_leases_identity_all_or_none",
        "(lease_owner_id IS NULL AND lease_owner_incarnation_id IS NULL AND lease_executor_id IS NULL AND lease_token IS NULL) OR (lease_owner_id IS NOT NULL AND lease_owner_incarnation_id IS NOT NULL AND lease_executor_id IS NOT NULL AND lease_token IS NOT NULL)",
    ),
    sqlite_constraint(
        SqliteConstraintDatabase::DurableCore,
        "session_meta",
        "ck_session_meta_relation_kind",
        "relation_kind IN ('root', 'child', 'fork')",
    ),
    sqlite_constraint(
        SqliteConstraintDatabase::DurableCore,
        "session_meta",
        "ck_session_meta_caused_by_kind",
        "caused_by_kind IN ('turn', 'effect_address', 'tool_call', 'process', 'process_event', 'trigger_occurrence', 'session_node')",
    ),
    sqlite_constraint(
        SqliteConstraintDatabase::DurableCore,
        "session_meta",
        "ck_session_meta_observer_inheritance_kind",
        "observer_inheritance_kind IN ('all', 'none', 'only')",
    ),
    sqlite_constraint(
        SqliteConstraintDatabase::ProcessRegistry,
        "processes",
        "ck_processes_status",
        "status IN ('running', 'waiting', 'completed', 'failed', 'cancelled', 'abandoned', 'caller_departed')",
    ),
    sqlite_constraint(
        SqliteConstraintDatabase::ProcessRegistry,
        "process_wake_deliveries",
        "ck_process_wake_deliveries_state",
        "state IN ('pending', 'enqueuing', 'enqueued', 'discarded')",
    ),
    sqlite_constraint(
        SqliteConstraintDatabase::ProcessRegistry,
        "process_wake_deliveries",
        "ck_process_wake_deliveries_discard_reason",
        "discard_reason IN ('expired', 'target_gone', 'retargeted', 'sequence_rewound')",
    ),
    sqlite_constraint(
        SqliteConstraintDatabase::ProcessRegistry,
        "tool_intent_submissions",
        "ck_tool_intent_submissions_kind",
        "kind IN ('start_process', 'signal_process', 'cancel_process', 'emit_process_event', 'emit_trigger')",
    ),
    sqlite_constraint(
        SqliteConstraintDatabase::Triggers,
        "trigger_subscriptions",
        "ck_trigger_subscriptions_live_enabled",
        "NOT (enabled AND tombstoned)",
    ),
    sqlite_constraint(
        SqliteConstraintDatabase::EffectReplay,
        "runtime_effect_replay",
        "ck_runtime_effect_replay_status",
        "status IN ('in_progress', 'completed', 'failed')",
    ),
];

/// Named `CHECK`s required from the published PostgreSQL schema.
pub const POSTGRES_EXPECTED_CONSTRAINTS: &[ExpectedConstraint] = &[
    expected_constraint(
        "lash_pending_turn_inputs",
        "ck_pending_turn_inputs_state",
        "state IN ('pending_active', 'deferred_next_turn', 'accepted', 'cancelled', 'completed')",
    ),
    expected_constraint(
        "lash_pending_turn_inputs",
        "ck_pending_turn_inputs_state_ingress",
        "((ingress_json::jsonb ->> 'scope') = 'active_turn' AND state IN ('pending_active', 'accepted', 'cancelled', 'completed')) OR ((ingress_json::jsonb ->> 'scope') = 'next_turn' AND state IN ('deferred_next_turn', 'cancelled', 'completed'))",
    ),
    expected_constraint(
        "lash_pending_turn_inputs",
        "ck_pending_turn_inputs_claim_id_token_all_or_none",
        "(claim_id IS NULL AND claim_token IS NULL) OR (claim_id IS NOT NULL AND claim_token IS NOT NULL)",
    ),
    expected_constraint(
        "lash_queued_work_batches",
        "ck_queued_work_batches_work_kind",
        "work_kind IN ('turn', 'control')",
    ),
    expected_constraint(
        "lash_queued_work_batches",
        "ck_queued_work_batches_delivery_policy",
        "delivery_policy IN ('earliest_safe_boundary', 'after_current_turn_commit')",
    ),
    expected_constraint(
        "lash_queued_work_batches",
        "ck_queued_work_batches_claim_id_token_all_or_none",
        "(claim_id IS NULL AND claim_token IS NULL) OR (claim_id IS NOT NULL AND claim_token IS NOT NULL)",
    ),
    expected_constraint(
        "lash_session_execution_leases",
        "ck_session_execution_leases_identity_all_or_none",
        "(lease_owner_id IS NULL AND lease_owner_incarnation_id IS NULL AND lease_executor_id IS NULL AND lease_token IS NULL) OR (lease_owner_id IS NOT NULL AND lease_owner_incarnation_id IS NOT NULL AND lease_executor_id IS NOT NULL AND lease_token IS NOT NULL)",
    ),
    expected_constraint(
        "lash_session_meta",
        "ck_session_meta_relation_kind",
        "relation_kind IN ('root', 'child', 'fork')",
    ),
    expected_constraint(
        "lash_session_meta",
        "ck_session_meta_caused_by_kind",
        "caused_by_kind IN ('turn', 'effect_address', 'tool_call', 'process', 'process_event', 'trigger_occurrence', 'session_node')",
    ),
    expected_constraint(
        "lash_session_meta",
        "ck_session_meta_observer_inheritance_kind",
        "observer_inheritance_kind IN ('all', 'none', 'only')",
    ),
    expected_constraint(
        "lash_processes",
        "ck_processes_status",
        "status IN ('running', 'waiting', 'completed', 'failed', 'cancelled', 'abandoned', 'caller_departed')",
    ),
    expected_constraint(
        "lash_process_wake_deliveries",
        "ck_process_wake_deliveries_state",
        "state IN ('pending', 'enqueuing', 'enqueued', 'discarded')",
    ),
    expected_constraint(
        "lash_process_wake_deliveries",
        "ck_process_wake_deliveries_discard_reason",
        "discard_reason IN ('expired', 'target_gone', 'retargeted', 'sequence_rewound')",
    ),
    expected_constraint(
        "lash_tool_intent_submissions",
        "ck_tool_intent_submissions_kind",
        "kind IN ('start_process', 'signal_process', 'cancel_process', 'emit_process_event', 'emit_trigger')",
    ),
    expected_constraint(
        "lash_trigger_subscriptions",
        "ck_trigger_subscriptions_live_enabled",
        "NOT (enabled AND tombstoned)",
    ),
    expected_constraint(
        "lash_runtime_effect_replay",
        "ck_runtime_effect_replay_status",
        "status IN ('in_progress', 'completed', 'failed')",
    ),
];

/// One required named `CHECK` that did not match the published definition.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RequiredConstraintFinding {
    Missing {
        table: String,
        name: String,
        expected_expression: String,
    },
    Altered {
        table: String,
        name: String,
        expected_expression: String,
        actual_expression: String,
    },
    Unvalidated {
        table: String,
        name: String,
    },
    Unenforced {
        table: String,
        name: String,
    },
}

/// Result of one explicit read-only inspection of registered named `CHECK`s.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequiredConstraintReport {
    findings: Vec<RequiredConstraintFinding>,
}

impl RequiredConstraintReport {
    /// Whether every registered named `CHECK` matched in the inspected snapshot.
    ///
    /// This does not establish schema-version compatibility, database
    /// openability, the state of unregistered constraints, or row integrity.
    pub fn is_conformant(&self) -> bool {
        self.findings.is_empty()
    }

    /// Missing, altered, unvalidated, and unenforced required checks.
    pub fn findings(&self) -> &[RequiredConstraintFinding] {
        &self.findings
    }
}

/// One live named `CHECK` read by a store adapter.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectedConstraint {
    pub table: String,
    pub name: String,
    pub expression: String,
    pub validated: bool,
    pub enforced: bool,
}

/// Compare live checks with the single published registry.
#[doc(hidden)]
pub fn compare_required_constraints(
    backend: &'static str,
    expected: &[ExpectedConstraint],
    actual: Vec<InspectedConstraint>,
) -> Result<RequiredConstraintReport, StoreError> {
    let mut actual_by_name = BTreeMap::new();
    for constraint in actual {
        let key = (constraint.table.clone(), constraint.name.clone());
        if actual_by_name.insert(key, constraint).is_some() {
            return Err(StoreError::RequiredConstraintInspectionInconclusive {
                backend,
                table: "<catalog>".to_string(),
                constraint: "<duplicate name>".to_string(),
                detail: "the catalog returned a duplicate table/constraint identity".to_string(),
            });
        }
    }

    let mut findings = Vec::new();
    for expected in expected {
        let key = (expected.table.to_string(), expected.name.to_string());
        let Some(actual) = actual_by_name.remove(&key) else {
            findings.push(RequiredConstraintFinding::Missing {
                table: expected.table.to_string(),
                name: expected.name.to_string(),
                expected_expression: expected.expression.to_string(),
            });
            continue;
        };
        let expected_ast =
            parse_expression_for_backend(backend, expected.expression).map_err(|detail| {
                StoreError::RequiredConstraintInspectionInconclusive {
                    backend,
                    table: expected.table.to_string(),
                    constraint: expected.name.to_string(),
                    detail: format!(
                        "published expression is outside the supported grammar: {detail}"
                    ),
                }
            })?;
        let actual_ast =
            parse_expression_for_backend(backend, &actual.expression).map_err(|detail| {
                StoreError::RequiredConstraintInspectionInconclusive {
                    backend,
                    table: expected.table.to_string(),
                    constraint: expected.name.to_string(),
                    detail: format!("live expression is outside the supported grammar: {detail}"),
                }
            })?;
        if expected_ast != actual_ast {
            findings.push(RequiredConstraintFinding::Altered {
                table: expected.table.to_string(),
                name: expected.name.to_string(),
                expected_expression: expected.expression.to_string(),
                actual_expression: actual.expression,
            });
        }
        if !actual.validated {
            findings.push(RequiredConstraintFinding::Unvalidated {
                table: expected.table.to_string(),
                name: expected.name.to_string(),
            });
        }
        if !actual.enforced {
            findings.push(RequiredConstraintFinding::Unenforced {
                table: expected.table.to_string(),
                name: expected.name.to_string(),
            });
        }
    }
    Ok(RequiredConstraintReport { findings })
}

/// Extract named `CHECK` bodies from one SQLite `CREATE TABLE` statement.
#[doc(hidden)]
pub fn extract_named_check_expressions(source: &str) -> Result<BTreeMap<String, String>, String> {
    let tokens = lex_sqlite_ddl(source)?;
    let mut checks = BTreeMap::new();
    let opening = sqlite_create_table_body_opening(&tokens)?;
    let mut item_start = opening + 1;
    let mut depth = 1_usize;
    for index in opening + 1..tokens.len() {
        match tokens[index].kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => {
                depth -= 1;
                if depth == 0 {
                    extract_checks_from_sqlite_table_item(
                        source,
                        &tokens[item_start..index],
                        &mut checks,
                    )?;
                    return Ok(checks);
                }
            }
            TokenKind::Comma if depth == 1 => {
                extract_checks_from_sqlite_table_item(
                    source,
                    &tokens[item_start..index],
                    &mut checks,
                )?;
                item_start = index + 1;
            }
            _ => {}
        }
    }
    Err("CREATE TABLE statement has no closing `)`".to_string())
}

fn sqlite_create_table_body_opening(tokens: &[Token]) -> Result<usize, String> {
    let mut index = 0;
    if !tokens
        .get(index)
        .is_some_and(|token| token.is_ident("create"))
    {
        return Err("schema SQL is not an ordinary `CREATE TABLE` statement".to_string());
    }
    index += 1;
    if tokens
        .get(index)
        .is_some_and(|token| token.is_ident("virtual"))
    {
        return Err("virtual tables do not have an ordinary `CREATE TABLE` body".to_string());
    }
    if !tokens
        .get(index)
        .is_some_and(|token| token.is_ident("table"))
    {
        return Err("schema SQL is not an ordinary `CREATE TABLE` statement".to_string());
    }
    index += 1;
    if tokens.get(index).is_some_and(|token| token.is_ident("if")) {
        if !tokens
            .get(index + 1)
            .is_some_and(|token| token.is_ident("not"))
            || !tokens
                .get(index + 2)
                .is_some_and(|token| token.is_ident("exists"))
        {
            return Err("malformed `CREATE TABLE IF NOT EXISTS` header".to_string());
        }
        index += 3;
    }
    if tokens.get(index).and_then(Token::identifier).is_none() {
        return Err("ordinary `CREATE TABLE` header has no table name".to_string());
    }
    index += 1;
    if !tokens
        .get(index)
        .is_some_and(|token| token.kind == TokenKind::LParen)
    {
        return Err("ordinary `CREATE TABLE` header has no column-definition body".to_string());
    }
    Ok(index)
}

fn extract_checks_from_sqlite_table_item(
    source: &str,
    tokens: &[Token],
    checks: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let mut depth = 0_usize;
    let mut index = 0_usize;
    while index < tokens.len() {
        match tokens[index].kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => depth = depth.saturating_sub(1),
            _ if depth == 0 && tokens[index].is_ident("constraint") => {
                let Some(name) = tokens.get(index + 1).and_then(Token::identifier) else {
                    index += 1;
                    continue;
                };
                if !tokens
                    .get(index + 2)
                    .is_some_and(|token| token.is_ident("check"))
                    || !tokens
                        .get(index + 3)
                        .is_some_and(|token| token.kind == TokenKind::LParen)
                {
                    index += 1;
                    continue;
                }
                let body_start = tokens[index + 3].end;
                let mut check_depth = 1_usize;
                let mut closing_index = None;
                for (offset, token) in tokens[index + 4..].iter().enumerate() {
                    match token.kind {
                        TokenKind::LParen => check_depth += 1,
                        TokenKind::RParen => {
                            check_depth -= 1;
                            if check_depth == 0 {
                                closing_index = Some(index + 4 + offset);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let closing_index = closing_index
                    .ok_or_else(|| format!("constraint `{name}` has no closing `)`"))?;
                if checks
                    .insert(
                        name.to_string(),
                        source[body_start..tokens[closing_index].start]
                            .trim()
                            .to_string(),
                    )
                    .is_some()
                {
                    return Err(format!("constraint name `{name}` appears more than once"));
                }
                index = closing_index;
            }
            _ => {}
        }
        index += 1;
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Expr {
    Identifier(SqlIdentifier),
    String(String),
    Number(String),
    Boolean(bool),
    Cast(Box<Self>, SqlIdentifier),
    Call(SqlIdentifier, Vec<Self>),
    JsonText(Box<Self>, Box<Self>),
    Not(Box<Self>),
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
    Compare(Box<Self>, Comparison, Box<Self>),
    IsNull(Box<Self>, bool),
    In(Box<Self>, Vec<Self>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SqlIdentifier {
    Folded(String),
    Exact(String),
}

impl SqlIdentifier {
    fn unquoted(value: String) -> Self {
        Self::Folded(value)
    }

    fn quoted(value: String) -> Self {
        if is_unquoted_identifier(&value) && value == value.to_ascii_lowercase() {
            Self::Folded(value)
        } else {
            Self::Exact(value)
        }
    }
}

fn is_unquoted_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Comparison {
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Token {
    kind: TokenKind,
    start: usize,
    end: usize,
}

impl Token {
    fn is_ident(&self, expected: &str) -> bool {
        matches!(&self.kind, TokenKind::Ident(found) if found.eq_ignore_ascii_case(expected))
    }

    fn identifier(&self) -> Option<&str> {
        match &self.kind {
            TokenKind::Ident(value) | TokenKind::QuotedIdent(value) => Some(value),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TokenKind {
    Ident(String),
    QuotedIdent(String),
    String(String),
    Number(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Cast,
    JsonText,
    Comparison(Comparison),
    Other(char),
}

fn lex_sqlite_ddl(source: &str) -> Result<Vec<Token>, String> {
    lex_with_mode(source, LexMode::Sqlite)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LexMode {
    PostgresExpression,
    Sqlite,
}

fn lex_with_mode(source: &str, mode: LexMode) -> Result<Vec<Token>, String> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if source[index..].starts_with("--") {
            index = source[index..]
                .find('\n')
                .map_or(bytes.len(), |end| index + end + 1);
            continue;
        }
        if source[index..].starts_with("/*") {
            let Some(end) = source[index + 2..].find("*/") else {
                return Err("unterminated block comment".to_string());
            };
            index += end + 4;
            continue;
        }
        let start = index;
        let kind = match byte {
            b'\'' => {
                index += 1;
                let mut value = String::new();
                loop {
                    let Some(next) = bytes.get(index).copied() else {
                        return Err("unterminated string literal".to_string());
                    };
                    if next == b'\'' {
                        if bytes.get(index + 1) == Some(&b'\'') {
                            value.push('\'');
                            index += 2;
                        } else {
                            index += 1;
                            break;
                        }
                    } else {
                        let character = source[index..]
                            .chars()
                            .next()
                            .ok_or_else(|| "invalid UTF-8 boundary".to_string())?;
                        value.push(character);
                        index += character.len_utf8();
                    }
                }
                TokenKind::String(value)
            }
            b'"' | b'`' => {
                let closing = byte;
                index += 1;
                let mut value = String::new();
                loop {
                    let Some(next) = bytes.get(index).copied() else {
                        return Err("unterminated quoted identifier".to_string());
                    };
                    if next == closing {
                        if bytes.get(index + 1) == Some(&closing) && closing != b']' {
                            value.push(closing as char);
                            index += 2;
                        } else {
                            index += 1;
                            break;
                        }
                    } else {
                        let character = source[index..]
                            .chars()
                            .next()
                            .ok_or_else(|| "invalid UTF-8 boundary".to_string())?;
                        value.push(character);
                        index += character.len_utf8();
                    }
                }
                TokenKind::QuotedIdent(if mode == LexMode::Sqlite {
                    value.to_ascii_lowercase()
                } else {
                    value
                })
            }
            b'(' => {
                index += 1;
                TokenKind::LParen
            }
            b')' => {
                index += 1;
                TokenKind::RParen
            }
            b'[' if mode == LexMode::Sqlite => {
                index += 1;
                let mut value = String::new();
                loop {
                    let Some(next) = bytes.get(index).copied() else {
                        return Err("unterminated bracket-quoted identifier".to_string());
                    };
                    if next == b']' {
                        index += 1;
                        break;
                    }
                    let character = source[index..]
                        .chars()
                        .next()
                        .ok_or_else(|| "invalid UTF-8 boundary".to_string())?;
                    value.push(character);
                    index += character.len_utf8();
                }
                TokenKind::QuotedIdent(value.to_ascii_lowercase())
            }
            b'[' => {
                index += 1;
                TokenKind::LBracket
            }
            b']' => {
                index += 1;
                TokenKind::RBracket
            }
            b',' => {
                index += 1;
                TokenKind::Comma
            }
            b':' if bytes.get(index + 1) == Some(&b':') => {
                index += 2;
                TokenKind::Cast
            }
            b'-' if bytes.get(index + 1) == Some(&b'>') && bytes.get(index + 2) == Some(&b'>') => {
                index += 3;
                TokenKind::JsonText
            }
            b'=' => {
                index += 1;
                TokenKind::Comparison(Comparison::Equal)
            }
            b'!' if bytes.get(index + 1) == Some(&b'=') => {
                index += 2;
                TokenKind::Comparison(Comparison::NotEqual)
            }
            b'<' if bytes.get(index + 1) == Some(&b'=') => {
                index += 2;
                TokenKind::Comparison(Comparison::LessEqual)
            }
            b'>' if bytes.get(index + 1) == Some(&b'=') => {
                index += 2;
                TokenKind::Comparison(Comparison::GreaterEqual)
            }
            b'<' if bytes.get(index + 1) == Some(&b'>') => {
                index += 2;
                TokenKind::Comparison(Comparison::NotEqual)
            }
            b'<' => {
                index += 1;
                TokenKind::Comparison(Comparison::Less)
            }
            b'>' => {
                index += 1;
                TokenKind::Comparison(Comparison::Greater)
            }
            b'0'..=b'9' => {
                index += 1;
                while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                    index += 1;
                }
                TokenKind::Number(source[start..index].to_string())
            }
            _ if byte.is_ascii_alphabetic() || byte == b'_' => {
                index += 1;
                while bytes
                    .get(index)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                {
                    index += 1;
                }
                TokenKind::Ident(source[start..index].to_ascii_lowercase())
            }
            _ => {
                let character = source[index..]
                    .chars()
                    .next()
                    .ok_or_else(|| "invalid UTF-8 boundary".to_string())?;
                index += character.len_utf8();
                TokenKind::Other(character)
            }
        };
        tokens.push(Token {
            kind,
            start,
            end: index,
        });
    }
    Ok(tokens)
}

#[cfg(test)]
fn parse_expression(source: &str) -> Result<Expr, String> {
    parse_expression_with_mode(source, LexMode::PostgresExpression)
}

fn parse_expression_for_backend(backend: &str, source: &str) -> Result<Expr, String> {
    let mode = if backend == "sqlite" {
        LexMode::Sqlite
    } else {
        LexMode::PostgresExpression
    };
    parse_expression_with_mode(source, mode)
}

fn parse_expression_with_mode(source: &str, mode: LexMode) -> Result<Expr, String> {
    let tokens = lex_with_mode(source, mode)?;
    let mut parser = Parser { tokens, index: 0 };
    let expression = parser.parse_or()?;
    if parser.index != parser.tokens.len() {
        return Err(format!(
            "unexpected token at byte {}",
            parser.tokens[parser.index].start
        ));
    }
    Ok(expression)
}

struct Parser {
    tokens: Vec<Token>,
    index: usize,
}

impl Parser {
    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut expression = self.parse_and()?;
        while self.consume_ident("or") {
            expression = Expr::Or(Box::new(expression), Box::new(self.parse_and()?));
        }
        Ok(expression)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut expression = self.parse_not()?;
        while self.consume_ident("and") {
            expression = Expr::And(Box::new(expression), Box::new(self.parse_not()?));
        }
        Ok(expression)
    }

    fn parse_not(&mut self) -> Result<Expr, String> {
        if self.consume_ident("not") {
            return Ok(Expr::Not(Box::new(self.parse_not()?)));
        }
        self.parse_predicate()
    }

    fn parse_predicate(&mut self) -> Result<Expr, String> {
        let left = self.parse_value()?;
        if self.consume_ident("is") {
            let negated = self.consume_ident("not");
            self.expect_ident("null")?;
            return Ok(Expr::IsNull(Box::new(left), negated));
        }
        if self.consume_ident("in") {
            self.expect(TokenKind::LParen)?;
            let values = self.parse_list(TokenKind::RParen)?;
            return Ok(Expr::In(Box::new(left), values));
        }
        let Some(TokenKind::Comparison(comparison)) = self.peek().map(|token| token.kind.clone())
        else {
            return Ok(left);
        };
        self.index += 1;
        if comparison == Comparison::Equal && self.consume_ident("any") {
            self.expect(TokenKind::LParen)?;
            self.expect_ident("array")?;
            self.expect(TokenKind::LBracket)?;
            let values = self.parse_list(TokenKind::RBracket)?;
            self.expect(TokenKind::RParen)?;
            return Ok(Expr::In(Box::new(left), values));
        }
        Ok(Expr::Compare(
            Box::new(left),
            comparison,
            Box::new(self.parse_value()?),
        ))
    }

    fn parse_list(&mut self, closing: TokenKind) -> Result<Vec<Expr>, String> {
        let mut values = Vec::new();
        if self.consume(closing.clone()) {
            return Ok(values);
        }
        loop {
            values.push(self.parse_value()?);
            if self.consume(closing.clone()) {
                return Ok(values);
            }
            self.expect(TokenKind::Comma)?;
        }
    }

    fn parse_value(&mut self) -> Result<Expr, String> {
        let mut value = if self.consume(TokenKind::LParen) {
            let value = self.parse_or()?;
            self.expect(TokenKind::RParen)?;
            value
        } else {
            let token = self
                .tokens
                .get(self.index)
                .cloned()
                .ok_or_else(|| "expected expression, found end of input".to_string())?;
            self.index += 1;
            match token.kind {
                TokenKind::Ident(identifier) if identifier == "true" => Expr::Boolean(true),
                TokenKind::Ident(identifier) if identifier == "false" => Expr::Boolean(false),
                TokenKind::Ident(identifier) => {
                    let identifier = SqlIdentifier::unquoted(identifier);
                    if self.consume(TokenKind::LParen) {
                        let arguments = self.parse_list(TokenKind::RParen)?;
                        Expr::Call(identifier, arguments)
                    } else {
                        Expr::Identifier(identifier)
                    }
                }
                TokenKind::QuotedIdent(identifier) => {
                    let identifier = SqlIdentifier::quoted(identifier);
                    if self.consume(TokenKind::LParen) {
                        let arguments = self.parse_list(TokenKind::RParen)?;
                        Expr::Call(identifier, arguments)
                    } else {
                        Expr::Identifier(identifier)
                    }
                }
                TokenKind::String(value) => Expr::String(value),
                TokenKind::Number(value) => Expr::Number(value),
                _ => {
                    return Err(format!(
                        "unsupported expression token at byte {}",
                        token.start
                    ));
                }
            }
        };
        loop {
            if self.consume(TokenKind::Cast) {
                let cast_token = self
                    .tokens
                    .get(self.index)
                    .map(|token| token.kind.clone())
                    .ok_or_else(|| "expected cast type".to_string())?;
                self.index += 1;
                match cast_token {
                    TokenKind::Ident(cast)
                        if cast == "text" && matches!(value, Expr::String(_)) => {}
                    TokenKind::Ident(cast) => {
                        value = Expr::Cast(Box::new(value), SqlIdentifier::unquoted(cast));
                    }
                    TokenKind::QuotedIdent(cast) => {
                        value = Expr::Cast(Box::new(value), SqlIdentifier::Exact(cast));
                    }
                    _ => return Err("expected cast type".to_string()),
                }
            } else if self.consume(TokenKind::JsonText) {
                value = Expr::JsonText(Box::new(value), Box::new(self.parse_value()?));
            } else {
                break;
            }
        }
        Ok(value)
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.index)
    }

    fn consume_ident(&mut self, identifier: &str) -> bool {
        if self.peek().is_some_and(|token| token.is_ident(identifier)) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn expect_ident(&mut self, identifier: &str) -> Result<(), String> {
        if self.consume_ident(identifier) {
            Ok(())
        } else {
            Err(format!("expected `{identifier}`"))
        }
    }

    fn consume(&mut self, kind: TokenKind) -> bool {
        if self.peek().is_some_and(|token| token.kind == kind) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: TokenKind) -> Result<(), String> {
        if self.consume(kind.clone()) {
            Ok(())
        } else {
            Err(format!("expected {kind:?}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_deparser_rewrites_compare_as_equal() {
        let source = "state IN ('pending', 'done')";
        let deparsed = "(state = ANY (ARRAY['pending'::text, 'done'::text]))";
        assert_eq!(parse_expression(source), parse_expression(deparsed));
    }

    #[test]
    fn quoted_identifier_comparison_preserves_sql_identity() {
        let expected = parse_expression("state IN ('pending', 'done')").unwrap();
        let quoted_lowercase = parse_expression("\"state\" IN ('pending', 'done')").unwrap();
        let quoted_uppercase = parse_expression("\"STATE\" IN ('pending', 'done')").unwrap();

        assert_eq!(expected, quoted_lowercase);
        assert_ne!(expected, quoted_uppercase);
        assert_ne!(
            parse_expression("'pending'::text").unwrap(),
            parse_expression("'pending'::\"text\"").unwrap(),
            "only PostgreSQL's unquoted built-in text cast may be discarded"
        );
    }

    #[test]
    fn parser_preserves_grouping_literal_operator_and_cast_changes() {
        let expected = parse_expression("(a = 'x' AND b = 'y') OR c = 'z'").unwrap();
        for altered in [
            "a = 'x' AND (b = 'y' OR c = 'z')",
            "(a = 'X' AND b = 'y') OR c = 'z'",
            "(a <> 'x' AND b = 'y') OR c = 'z'",
            "(a::text = 'x' AND b = 'y') OR c = 'z'",
        ] {
            assert_ne!(expected, parse_expression(altered).unwrap(), "{altered}");
        }
    }

    #[test]
    fn extraction_ignores_comments_and_preserves_quoted_literals() {
        let ddl = r#"CREATE TABLE example (
            value TEXT,
            CONSTRAINT ck_example CHECK (
                value IN ('comma,paren)', 'quote''inside') /* comment ) */
            )
        )"#;
        let found = extract_named_check_expressions(ddl).unwrap();
        assert_eq!(
            parse_expression(&found["ck_example"]),
            parse_expression("value IN ('comma,paren)', 'quote''inside')")
        );
    }

    #[test]
    fn sqlite_extraction_requires_a_real_declaration_keyword() {
        let forged = r#"CREATE TABLE example (
            "constraint" ck_example CHECK(value = 'ok'),
            [CONSTRAINT ck_bracket CHECK (value = 'ok')] TEXT,
            value TEXT CHECK (
                coalesce(value, 'CONSTRAINT ck_nested CHECK (value = ''ok'')') <> ''
            )
        )"#;
        assert!(extract_named_check_expressions(forged).unwrap().is_empty());

        let genuine = r#"CREATE TABLE example (
            value TEXT CONSTRAINT "ck_column" CHECK (value = 'column'),
            CONSTRAINT `ck_table` CHECK (value = 'table')
        )"#;
        let found = extract_named_check_expressions(genuine).unwrap();
        assert_eq!(found["ck_column"], "value = 'column'");
        assert_eq!(found["ck_table"], "value = 'table'");
    }

    #[test]
    fn sqlite_extraction_rejects_virtual_table_module_arguments() {
        let virtual_table = "CREATE VIRTUAL TABLE runtime_effect_replay USING rtree(\
            id, min, max, +status CONSTRAINT ck_runtime_effect_replay_status \
            CHECK(status IN ('in_progress', 'completed', 'failed')))";
        assert!(
            extract_named_check_expressions(virtual_table)
                .unwrap_err()
                .contains("virtual tables")
        );
    }

    #[test]
    fn unsupported_syntax_is_inconclusive() {
        let expected = [expected_constraint("t", "ck", "value = 'ok'")];
        let error = compare_required_constraints(
            "test",
            &expected,
            vec![InspectedConstraint {
                table: "t".to_string(),
                name: "ck".to_string(),
                expression: "pg_catalog.lower(value) = 'ok'".to_string(),
                validated: true,
                enforced: true,
            }],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            StoreError::RequiredConstraintInspectionInconclusive { .. }
        ));
    }

    #[test]
    fn queued_work_predecessor_pairs_are_typed_and_complete() {
        for (claim_id, claim_token) in [
            (Some("claim".to_string()), None),
            (None, Some("token".to_string())),
        ] {
            let error = crate::store_backend_support::queued_work_claim_data(
                Vec::new(),
                claim_id,
                claim_token,
            )
            .unwrap_err();
            assert!(matches!(
                error,
                StoreError::QueuedWorkPredecessorClaimCorrupt { .. }
            ));
        }

        for (claim_id, claim_token) in [
            (None, None),
            (Some("claim".to_string()), Some("token".to_string())),
        ] {
            let data = crate::store_backend_support::queued_work_claim_data(
                Vec::new(),
                claim_id.clone(),
                claim_token.clone(),
            )
            .expect("complete predecessor pairs are valid");
            assert_eq!(data.abandon_restore_claim_id, claim_id);
            assert_eq!(
                data.abandon_restore_claim_token.as_deref(),
                claim_token.as_deref()
            );
        }
    }
}
