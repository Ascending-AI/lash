//! The admin API's `sys_invocation` table, queried with the small SQL subset
//! lash sends: `SELECT cols | COUNT(1) FROM sys_invocation [WHERE ...]
//! [GROUP BY col] [ORDER BY col [ASC|DESC]] [LIMIT n]`, where conditions are
//! `=`, `!=`, `IN (...)`, `LIKE 'prefix%'` and `IS [NOT] NULL` joined by
//! `AND`/`OR` and parentheses. Anything else is refused loudly rather than
//! answered wrong.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::catalog::ServiceKind;
use super::model::{Invocation, Outcome, Status};
use super::processor::State;

/// Run `sql` over the server's invocations.
pub(super) fn run(state: &State, sql: &str) -> Result<Vec<Value>, String> {
    let query = Parser::new(sql)?.query()?;
    let mut rows: Vec<Map<String, Value>> = state
        .invocations
        .iter()
        .map(|invocation| row(state, invocation))
        .filter(|row| query.filter.as_ref().is_none_or(|filter| filter.eval(row)))
        .collect();
    if let Some((column, descending)) = &query.order_by {
        rows.sort_by(|left, right| compare(left.get(column), right.get(column)));
        if *descending {
            rows.reverse();
        }
    }
    let mut output: Vec<Value> = match &query.group_by {
        Some(group) => {
            let mut groups: BTreeMap<String, (Value, u64)> = BTreeMap::new();
            for row in &rows {
                let value = row.get(group).cloned().unwrap_or(Value::Null);
                groups.entry(value.to_string()).or_insert((value, 0)).1 += 1;
            }
            groups
                .into_values()
                .map(|(value, count)| {
                    let mut out = Map::new();
                    for item in &query.select {
                        match item {
                            Select::Count(alias) => {
                                out.insert(alias.clone(), json!(count));
                            }
                            Select::Column(column, alias) if column == group => {
                                out.insert(alias.clone(), value.clone());
                            }
                            Select::Column(..) | Select::All => {}
                        }
                    }
                    Value::Object(out)
                })
                .collect()
        }
        None if query
            .select
            .iter()
            .any(|item| matches!(item, Select::Count(_))) =>
        {
            let mut out = Map::new();
            for item in &query.select {
                if let Select::Count(alias) = item {
                    out.insert(alias.clone(), json!(rows.len()));
                }
            }
            vec![Value::Object(out)]
        }
        None => rows
            .iter()
            .map(|row| {
                let mut out = Map::new();
                for item in &query.select {
                    match item {
                        Select::All => out.extend(row.clone()),
                        Select::Column(column, alias) => {
                            out.insert(
                                alias.clone(),
                                row.get(column).cloned().unwrap_or(Value::Null),
                            );
                        }
                        Select::Count(_) => {}
                    }
                }
                Value::Object(out)
            })
            .collect(),
    };
    if let Some(limit) = query.limit {
        output.truncate(limit);
    }
    Ok(output)
}

fn compare(left: Option<&Value>, right: Option<&Value>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(Value::Number(left)), Some(Value::Number(right))) => left
            .as_f64()
            .partial_cmp(&right.as_f64())
            .unwrap_or(std::cmp::Ordering::Equal),
        (left, right) => left.map(Value::to_string).cmp(&right.map(Value::to_string)),
    }
}

/// The `sys_invocation` status of an invocation.
fn status(invocation: &Invocation) -> &'static str {
    match &invocation.status {
        Status::Inboxed => "pending",
        Status::Scheduled => "scheduled",
        Status::Running(_) => "running",
        Status::Suspended(_) => "suspended",
        Status::BackingOff => "backing-off",
        Status::Paused => "paused",
        Status::Completed(_) => "completed",
    }
}

fn row(state: &State, invocation: &Invocation) -> Map<String, Value> {
    let (completion_result, completion_failure) = match &invocation.status {
        Status::Completed(Outcome::Success(_)) => (json!("success"), Value::Null),
        Status::Completed(Outcome::Failure(failure)) => (
            json!("failure"),
            json!(format!("[{}] {}", failure.code, failure.message)),
        ),
        _ => (Value::Null, Value::Null),
    };
    let parent = invocation
        .parent
        .map(|parent| state.invocations[parent.0].id.as_str().to_owned());
    let mut row = Map::new();
    row.insert("id".into(), json!(invocation.id.as_str()));
    row.insert("target".into(), json!(invocation.target.display()));
    row.insert(
        "target_service_name".into(),
        json!(invocation.target.service),
    );
    row.insert("target_service_key".into(), json!(invocation.target.key));
    row.insert(
        "target_handler_name".into(),
        json!(invocation.target.handler),
    );
    row.insert(
        "target_service_ty".into(),
        json!(match invocation.spec.service_kind {
            ServiceKind::Service => "service",
            ServiceKind::VirtualObject => "virtual_object",
            ServiceKind::Workflow => "workflow",
        }),
    );
    row.insert("status".into(), json!(status(invocation)));
    row.insert("completion_result".into(), completion_result);
    row.insert("completion_failure".into(), completion_failure);
    row.insert(
        "pinned_deployment_id".into(),
        if invocation.attempts > 0 {
            json!("dp_restate_test")
        } else {
            Value::Null
        },
    );
    row.insert("idempotency_key".into(), json!(invocation.idempotency_key));
    row.insert(
        "invoked_by".into(),
        json!(if parent.is_some() {
            "service"
        } else {
            "ingress"
        }),
    );
    row.insert("invoked_by_id".into(), json!(parent));
    row.insert("journal_size".into(), json!(invocation.journal.len()));
    row.insert(
        "retry_count".into(),
        json!(invocation.retry.failures_in_loop),
    );
    row.insert(
        "last_failure".into(),
        json!(
            invocation
                .retry
                .last_failure
                .as_ref()
                .map(|failure| format!("[{}] {}", failure.code, failure.message))
        ),
    );
    row.insert(
        "last_failure_error_code".into(),
        json!(
            invocation
                .retry
                .last_failure
                .as_ref()
                .map(|failure| failure.code.to_string())
        ),
    );
    row.insert("created_at".into(), json!(invocation.created_ms));
    row.insert("modified_at".into(), json!(invocation.modified_seq));
    row
}

// ---------------------------------------------------------------------------
// The SQL subset
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Select {
    All,
    Column(String, String),
    Count(String),
}

#[derive(Debug)]
enum Cond {
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
    Eq(String, Value, bool),
    In(String, Vec<Value>),
    Like(String, String),
    IsNull(String, bool),
}

impl Cond {
    fn eval(&self, row: &Map<String, Value>) -> bool {
        let get = |column: &String| row.get(column).unwrap_or(&Value::Null);
        match self {
            Self::And(left, right) => left.eval(row) && right.eval(row),
            Self::Or(left, right) => left.eval(row) || right.eval(row),
            Self::Eq(column, value, equal) => loose_eq(get(column), value) == *equal,
            Self::In(column, values) => values.iter().any(|value| loose_eq(get(column), value)),
            Self::Like(column, pattern) => {
                get(column).as_str().is_some_and(|text| like(text, pattern))
            }
            Self::IsNull(column, null) => get(column).is_null() == *null,
        }
    }
}

fn loose_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::String(right)) => left.to_string() == *right,
        (left, right) => left == right,
    }
}

/// SQL `LIKE` with `%` wildcards (no `_`).
fn like(text: &str, pattern: &str) -> bool {
    let parts: Vec<&str> = pattern.split('%').collect();
    let [first, middle @ .., last] = parts.as_slice() else {
        return text == pattern;
    };
    if parts.len() == 1 {
        return text == pattern;
    }
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    for part in middle {
        match rest.find(part) {
            Some(index) => rest = &rest[index + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

#[derive(Debug)]
struct Query {
    select: Vec<Select>,
    filter: Option<Cond>,
    group_by: Option<String>,
    order_by: Option<(String, bool)>,
    limit: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Word(String),
    Str(String),
    Num(u64),
    Sym(char),
    Ne,
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

impl Parser {
    fn new(sql: &str) -> Result<Self, String> {
        let chars: Vec<char> = sql.chars().collect();
        let mut tokens = Vec::new();
        let mut index = 0;
        while index < chars.len() {
            let character = chars[index];
            if character.is_whitespace() {
                index += 1;
            } else if character == '\'' {
                let mut text = String::new();
                index += 1;
                loop {
                    match chars.get(index) {
                        Some('\'') if chars.get(index + 1) == Some(&'\'') => {
                            text.push('\'');
                            index += 2;
                        }
                        Some('\'') => {
                            index += 1;
                            break;
                        }
                        Some(other) => {
                            text.push(*other);
                            index += 1;
                        }
                        None => return Err("unterminated string literal".into()),
                    }
                }
                tokens.push(Token::Str(text));
            } else if character.is_ascii_digit() {
                let start = index;
                while chars.get(index).is_some_and(char::is_ascii_digit) {
                    index += 1;
                }
                let digits: String = chars[start..index].iter().collect();
                tokens.push(Token::Num(
                    digits.parse().map_err(|_| format!("bad number {digits}"))?,
                ));
            } else if character.is_alphanumeric() || character == '_' {
                let start = index;
                while chars
                    .get(index)
                    .is_some_and(|next| next.is_alphanumeric() || *next == '_' || *next == '.')
                {
                    index += 1;
                }
                tokens.push(Token::Word(chars[start..index].iter().collect()));
            } else if (character == '!' || character == '<') && chars.get(index + 1).is_some() {
                let next = chars[index + 1];
                if (character == '!' && next == '=') || (character == '<' && next == '>') {
                    tokens.push(Token::Ne);
                    index += 2;
                } else {
                    return Err(format!("unsupported operator {character}{next}"));
                }
            } else if "(),=*;".contains(character) {
                tokens.push(Token::Sym(character));
                index += 1;
            } else {
                return Err(format!("unsupported character {character:?} in query"));
            }
        }
        Ok(Self { tokens, at: 0 })
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn keyword(&mut self, word: &str) -> bool {
        if matches!(self.peek(), Some(Token::Word(found)) if found.eq_ignore_ascii_case(word)) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect_keyword(&mut self, word: &str) -> Result<(), String> {
        if self.keyword(word) {
            Ok(())
        } else {
            Err(format!("expected {word}, found {:?}", self.peek()))
        }
    }

    fn symbol(&mut self, symbol: char) -> bool {
        if self.peek() == Some(&Token::Sym(symbol)) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn ident(&mut self) -> Result<String, String> {
        match self.tokens.get(self.at).cloned() {
            Some(Token::Word(word)) => {
                self.at += 1;
                Ok(word.to_ascii_lowercase())
            }
            other => Err(format!("expected a column, found {other:?}")),
        }
    }

    fn literal(&mut self) -> Result<Value, String> {
        match self.tokens.get(self.at).cloned() {
            Some(Token::Str(text)) => {
                self.at += 1;
                Ok(Value::String(text))
            }
            Some(Token::Num(number)) => {
                self.at += 1;
                Ok(json!(number))
            }
            other => Err(format!("expected a literal, found {other:?}")),
        }
    }

    fn query(mut self) -> Result<Query, String> {
        self.expect_keyword("select")?;
        let mut select = Vec::new();
        loop {
            if self.symbol('*') {
                select.push(Select::All);
            } else if self.keyword("count") {
                if !self.symbol('(') {
                    return Err("expected ( after COUNT".into());
                }
                if !self.symbol('*') {
                    self.literal()?;
                }
                if !self.symbol(')') {
                    return Err("expected ) after COUNT(".into());
                }
                let alias = if self.keyword("as") {
                    self.ident()?
                } else {
                    "count".into()
                };
                select.push(Select::Count(alias));
            } else {
                let column = self.ident()?;
                let alias = if self.keyword("as") {
                    self.ident()?
                } else {
                    column.clone()
                };
                select.push(Select::Column(column, alias));
            }
            if !self.symbol(',') {
                break;
            }
        }
        self.expect_keyword("from")?;
        let table = self.ident()?;
        if table != "sys_invocation" {
            return Err(format!(
                "the double serves only sys_invocation, not {table}"
            ));
        }
        let filter = if self.keyword("where") {
            Some(self.or()?)
        } else {
            None
        };
        let group_by = if self.keyword("group") {
            self.expect_keyword("by")?;
            Some(self.ident()?)
        } else {
            None
        };
        let order_by = if self.keyword("order") {
            self.expect_keyword("by")?;
            let column = self.ident()?;
            let descending = if self.keyword("desc") {
                true
            } else {
                self.keyword("asc");
                false
            };
            Some((column, descending))
        } else {
            None
        };
        let limit = if self.keyword("limit") {
            match self.literal()? {
                Value::Number(number) => number.as_u64().and_then(|n| usize::try_from(n).ok()),
                _ => return Err("LIMIT takes a number".into()),
            }
        } else {
            None
        };
        self.symbol(';');
        if self.at != self.tokens.len() {
            return Err(format!(
                "unsupported query tail {:?}",
                &self.tokens[self.at..]
            ));
        }
        Ok(Query {
            select,
            filter,
            group_by,
            order_by,
            limit,
        })
    }

    fn or(&mut self) -> Result<Cond, String> {
        let mut cond = self.and()?;
        while self.keyword("or") {
            cond = Cond::Or(Box::new(cond), Box::new(self.and()?));
        }
        Ok(cond)
    }

    fn and(&mut self) -> Result<Cond, String> {
        let mut cond = self.atom()?;
        while self.keyword("and") {
            cond = Cond::And(Box::new(cond), Box::new(self.atom()?));
        }
        Ok(cond)
    }

    fn atom(&mut self) -> Result<Cond, String> {
        if self.symbol('(') {
            let cond = self.or()?;
            if !self.symbol(')') {
                return Err("expected )".into());
            }
            return Ok(cond);
        }
        let column = self.ident()?;
        if self.symbol('=') {
            return Ok(Cond::Eq(column, self.literal()?, true));
        }
        if self.peek() == Some(&Token::Ne) {
            self.at += 1;
            return Ok(Cond::Eq(column, self.literal()?, false));
        }
        if self.keyword("in") {
            if !self.symbol('(') {
                return Err("expected ( after IN".into());
            }
            let mut values = vec![self.literal()?];
            while self.symbol(',') {
                values.push(self.literal()?);
            }
            if !self.symbol(')') {
                return Err("expected ) after IN list".into());
            }
            return Ok(Cond::In(column, values));
        }
        if self.keyword("like") {
            return match self.literal()? {
                Value::String(pattern) => Ok(Cond::Like(column, pattern)),
                _ => Err("LIKE takes a string".into()),
            };
        }
        if self.keyword("is") {
            let not = self.keyword("not");
            self.expect_keyword("null")?;
            return Ok(Cond::IsNull(column, !not));
        }
        Err(format!("unsupported condition on {column}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_query_lash_sends() {
        for sql in [
            "SELECT id, status FROM sys_invocation WHERE id = 'inv_1''x'",
            "SELECT id FROM sys_invocation WHERE target_service_name = 'A' AND target_service_key = 'k' AND target_handler_name = 'run' ORDER BY modified_at DESC LIMIT 1",
            "SELECT id, pinned_deployment_id FROM sys_invocation WHERE status IN ('pending', 'ready', 'running', 'backing-off', 'suspended') AND (target_service_name LIKE 'Lash%' OR target_service_name LIKE 'Effect%') ORDER BY modified_at DESC",
            "SELECT pinned_deployment_id, COUNT(1) as open_count FROM sys_invocation WHERE status IN ('pending', 'ready') GROUP BY pinned_deployment_id",
            "SELECT id FROM sys_invocation WHERE status = 'paused' AND last_failure IS NOT NULL",
        ] {
            Parser::new(sql).and_then(Parser::query).unwrap();
        }
        assert!(
            Parser::new("DELETE FROM sys_invocation")
                .and_then(Parser::query)
                .is_err()
        );
    }

    #[test]
    fn like_matches_prefixes_and_infixes() {
        assert!(like("LashProcessWorkflow", "Lash%"));
        assert!(like("LashProcessWorkflow", "%Process%"));
        assert!(!like("EffectGroupIndex", "Lash%"));
        assert!(like("exact", "exact"));
    }
}
