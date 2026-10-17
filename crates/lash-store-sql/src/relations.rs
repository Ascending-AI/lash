//! SQL-visible relation participation. This is deliberately not physical
//! per-table attribution: server function bodies, triggers and plans are absent.
use std::collections::BTreeSet;

/// The set of relation names visible in a statement, including DML targets.
/// Unknown/unsupported lexical forms return None, never an empty known set.
/// Local CTEs and table-valued functions are not physical relation names.
#[must_use]
pub fn relation_set(sql: &str) -> Option<Vec<String>> {
    let tokens = tokens(sql)?;
    if tokens.windows(2).any(|pair| {
        matches!(
            pair[0].as_str(),
            "extract" | "substring" | "trim" | "overlay"
        ) && pair[1] == "("
    }) {
        return None;
    }
    let mut locals = BTreeSet::new();
    for (i, word) in tokens.iter().enumerate() {
        let mut j = i + 1;
        // A CTE may declare its output columns before AS (recursive or not).
        if tokens.get(j).is_some_and(|v| v == "(") {
            let mut depth = 1;
            j += 1;
            while j < tokens.len() && depth != 0 {
                if tokens[j] == "(" {
                    depth += 1;
                }
                if tokens[j] == ")" {
                    depth -= 1;
                }
                j += 1;
            }
        }
        if tokens.get(j).is_some_and(|v| v == "as") {
            j += 1;
            if tokens.get(j).is_some_and(|v| v == "not") {
                j += 1;
            }
            if tokens.get(j).is_some_and(|v| v == "materialized") {
                j += 1;
            }
            if tokens.get(j).is_some_and(|v| v == "(") {
                locals.insert(word.trim_matches('"').to_owned());
            }
        }
    }
    let mut found = BTreeSet::new();
    let mut depth = 0_usize;
    let mut from = vec![false];
    let mut expect = false;
    let mut i = 0;
    while i < tokens.len() {
        let word = &tokens[i];
        if word == "(" {
            depth += 1;
            from.push(false);
            expect = false;
            i += 1;
            continue;
        }
        if word == ")" {
            if depth == 0 {
                return None;
            }
            depth -= 1;
            from.pop();
            expect = false;
            i += 1;
            continue;
        }
        if expect {
            if matches!(word.as_str(), "only" | "if" | "not" | "exists") {
                i += 1;
                continue;
            }
            let mut name = word.trim_matches('"').to_owned();
            let mut j = i + 1;
            while tokens.get(j).is_some_and(|v| v == ".") {
                name.push('.');
                name.push_str(tokens.get(j + 1)?.trim_matches('"'));
                j += 2;
            }
            let function = tokens.get(j).is_some_and(|v| v == "(")
                && tokens
                    .get(i.wrapping_sub(1))
                    .is_none_or(|v| v != "into" && v != "table");
            if !locals.contains(&name) && !function && name != "<literal>" {
                found.insert(name);
            }
            expect = false;
            i = j;
            continue;
        }
        if word == "from"
            && !tokens
                .get(i.wrapping_sub(1))
                .is_some_and(|v| v == "distinct")
        {
            from[depth] = true;
            expect = true;
        } else if matches!(word.as_str(), "join" | "into" | "references")
            || (word == "update"
                && !tokens
                    .get(i.wrapping_sub(1))
                    .is_some_and(|v| v == "do" || v == "for"))
            || (word == "table"
                && tokens.get(i.wrapping_sub(1)).is_some_and(|v| {
                    v == "create"
                        || v == "drop"
                        || v == "alter"
                        || v == "truncate"
                        || v == "temp"
                        || v == "temporary"
                        || v == "unlogged"
                }))
            || (word == "," && from[depth])
        {
            expect = true;
        } else if matches!(
            word.as_str(),
            "where" | "group" | "order" | "limit" | "returning" | "union" | "on" | "set" | ";"
        ) {
            from[depth] = false;
        }
        // SQL-visible relations do not reveal the tables inside server routines.
        if word.starts_with("lash_") && tokens.get(i + 1).is_some_and(|v| v == "(") {
            return None;
        }
        i += 1;
    }
    (depth == 0).then(|| found.into_iter().collect())
}

/// Conservative SELECT/WITH read syntax for repeat qualification. Hidden
/// server routines and data-modifying CTEs cannot be certified from a SELECT
/// command tag alone. This is not a cache-validity or server purity oracle.
#[must_use]
pub fn read_statement(sql: &str) -> Option<bool> {
    let words = tokens(sql)?;
    if !words.first().is_some_and(|w| w == "select" || w == "with") {
        return Some(false);
    }
    for (i, word) in words.iter().enumerate() {
        if matches!(
            word.as_str(),
            "insert" | "delete" | "merge" | "replace" | "call" | "copy"
        ) || (word == "update" && !words.get(i.wrapping_sub(1)).is_some_and(|v| v == "for"))
        {
            return Some(false);
        }
        if words.get(i + 1).is_some_and(|v| v == "(")
            && (word.starts_with("lash_")
                || word.starts_with("pg_advisory")
                || word.starts_with("pg_try_advisory")
                || matches!(word.as_str(), "nextval" | "setval" | "pg_notify"))
        {
            return None;
        }
    }
    Some(true)
}

fn tokens(sql: &str) -> Option<Vec<String>> {
    if sql.len() > 65_536 {
        return None;
    }
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        match bytes[i] {
            b if b.is_ascii_whitespace() => {
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                let mut depth = 1;
                while i + 1 < bytes.len() && depth > 0 {
                    if &bytes[i..i + 2] == b"/*" {
                        depth += 1;
                        i += 2;
                    } else if &bytes[i..i + 2] == b"*/" {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if depth != 0 {
                    return None;
                }
            }
            quote @ (b'\'' | b'"' | b'`' | b'[') => {
                let close = if quote == b'[' { b']' } else { quote };
                i += 1;
                let mut value = Vec::new();
                loop {
                    let b = *bytes.get(i)?;
                    i += 1;
                    if b == close {
                        if bytes.get(i) == Some(&close) {
                            value.push(b);
                            i += 1;
                        } else {
                            break;
                        }
                    } else {
                        value.push(b);
                    }
                }
                out.push(if quote == b'\'' {
                    "<literal>".into()
                } else {
                    format!("\"{}\"", String::from_utf8(value).ok()?)
                });
            }
            b'$' if bytes
                .get(i + 1)
                .is_some_and(|b| *b == b'$' || b.is_ascii_alphabetic() || *b == b'_') =>
            {
                return None;
            }
            b if b.is_ascii_alphabetic() || b == b'_' => {
                i += 1;
                while bytes
                    .get(i)
                    .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
                {
                    i += 1;
                }
                out.push(sql[start..i].to_ascii_lowercase());
            }
            b => {
                out.push(char::from(b).to_string());
                i += 1;
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// SQL-COST-JOIN: relation participation is a set; CTE aliases,
    /// comments and literal words must not become fictitious tables.
    #[test]
    fn relation_participation_excludes_ctes_and_literals() {
        assert_eq!(
            read_statement("WITH t AS (SELECT 1) SELECT * FROM t"),
            Some(true)
        );
        assert_eq!(
            read_statement("WITH t AS (DELETE FROM a RETURNING *) SELECT * FROM t"),
            Some(false)
        );
        assert_eq!(
            read_statement("SELECT 'delete' FROM a FOR UPDATE"),
            Some(true)
        );
        assert_eq!(read_statement("SELECT lash_owner_commit($1)"), None);

        assert_eq!(
            relation_set(
                "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<2) SELECT * FROM n"
            ),
            Some(vec![])
        );
        assert_eq!(
            relation_set("SELECT \"from\" FROM \"from\""),
            Some(vec!["from".into()])
        );

        assert_eq!(
            relation_set(
                "WITH t AS MATERIALIZED (SELECT * FROM main.accounts) SELECT 'from fake' FROM t JOIN \"items\" n ON true -- join fake\n"
            ),
            Some(vec!["items".into(), "main.accounts".into()])
        );
        assert_eq!(
            relation_set("SELECT * FROM a, b WHERE a.id=b.id"),
            Some(vec!["a".into(), "b".into()])
        );
        assert_eq!(
            relation_set("INSERT INTO a(id) VALUES(1) ON CONFLICT DO UPDATE SET id=2"),
            Some(vec!["a".into()])
        );
        assert_eq!(relation_set("SELECT lash_owner_commit($1)"), None);
        assert_eq!(relation_set("SELECT $$from fake$$"), None);
    }
}
