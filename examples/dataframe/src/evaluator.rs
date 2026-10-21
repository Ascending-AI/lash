//! Small, deterministic evaluator. Replace this module with a database adapter.
use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    pub rows: Vec<Value>,
    /// Column names, optionally followed by `:int64_text`.
    pub schema: Vec<String>,
    pub plan: Vec<Stage>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Stage {
    Filter {
        predicate: Expr,
    },
    Aggregate {
        keys: Vec<Expr>,
        aggregates: Vec<Expr>,
    },
    Sort {
        keys: Vec<Expr>,
    },
    Limit {
        n: usize,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Expr {
    pub op: String,
    pub args: Vec<Value>,
    pub name: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Failure {
    #[error("invalid dataframe plan: {0}")]
    InvalidPlan(String),
    #[error("dataframe execution was cancelled")]
    Cancelled,
    #[error("integer aggregate exceeds signed 64-bit range")]
    Overflow,
}

impl Failure {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidPlan(_) => "dataframe.invalid_plan",
            Self::Cancelled => "dataframe.cancelled",
            Self::Overflow => "dataframe.overflow",
        }
    }
}

fn invalid(message: impl Into<String>) -> Failure {
    Failure::InvalidPlan(message.into())
}

impl Expr {
    fn child(&self) -> Result<Self, Failure> {
        serde_json::from_value(
            self.args
                .first()
                .cloned()
                .ok_or_else(|| invalid("missing expression"))?,
        )
        .map_err(|e| invalid(e.to_string()))
    }

    fn value(&self, row: &Value, schema: &[String]) -> Result<Value, Failure> {
        match self.op.as_str() {
            "col" => {
                let name = self
                    .args
                    .first()
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("column name"))?;
                let index = schema
                    .iter()
                    .position(|s| s.split(':').next() == Some(name))
                    .ok_or_else(|| invalid(format!("unknown column {name}")))?;
                if let Some(values) = row.as_array() {
                    values
                        .get(index)
                        .cloned()
                        .ok_or_else(|| invalid("row width"))
                } else if index == 0 && schema.len() == 1 {
                    Ok(row.clone())
                } else {
                    Err(invalid("expected positional row"))
                }
            }
            "lit" => self
                .args
                .first()
                .cloned()
                .ok_or_else(|| invalid("literal value")),
            "alias" | "sort" => self.child()?.value(row, schema),
            "neq" => {
                let other: Expr = serde_json::from_value(
                    self.args
                        .get(1)
                        .cloned()
                        .ok_or_else(|| invalid("neq rhs"))?,
                )
                .map_err(|e| invalid(e.to_string()))?;
                Ok(Value::Bool(
                    self.child()?.value(row, schema)? != other.value(row, schema)?,
                ))
            }
            _ => Err(invalid(format!("unsupported expression {}", self.op))),
        }
    }

    fn integer(&self, row: &Value, schema: &[String]) -> Result<i64, Failure> {
        let value = self.value(row, schema)?;
        value
            .as_i64()
            .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
            .ok_or_else(|| invalid("expected signed 64-bit integer"))
    }

    fn aggregate(
        &self,
        rows: &[Value],
        schema: &[String],
        cancelled: &impl Fn() -> bool,
    ) -> Result<i64, Failure> {
        match self.op.as_str() {
            "alias" => self.child()?.aggregate(rows, schema, cancelled),
            "count" => i64::try_from(rows.len()).map_err(|_| Failure::Overflow),
            "sum" => {
                let expr = self.child()?;
                rows.iter().try_fold(0i64, |total, row| {
                    check(cancelled)?;
                    total
                        .checked_add(expr.integer(row, schema)?)
                        .ok_or(Failure::Overflow)
                })
            }
            _ => Err(invalid("aggregate must be count or sum")),
        }
    }

    fn sort_key(&self, row: &Value, schema: &[String]) -> Result<(SortValue, bool), Failure> {
        if self.op == "sort" {
            let ascending = self
                .args
                .get(1)
                .and_then(Value::as_bool)
                .ok_or_else(|| invalid("sort direction"))?;
            let (value, _) = self.child()?.sort_key(row, schema)?;
            return Ok((value, ascending));
        }
        // Counts are text on the wire; their declared encoding orders numerically.
        if self.op == "col"
            && schema.iter().any(|s| {
                s.strip_suffix(":int64_text") == self.args.first().and_then(Value::as_str)
                    && s.ends_with(":int64_text")
            })
        {
            return Ok((SortValue::Integer(self.integer(row, schema)?), true));
        }
        let value = match self.value(row, schema)? {
            Value::String(text) => SortValue::Text(text),
            Value::Number(number) => SortValue::Integer(
                number
                    .as_i64()
                    .ok_or_else(|| invalid("sort requires integers or text"))?,
            ),
            _ => return Err(invalid("sort requires integers or text")),
        };
        Ok((value, true))
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum SortValue {
    Integer(i64),
    Text(String),
}

fn check(cancelled: &impl Fn() -> bool) -> Result<(), Failure> {
    if cancelled() {
        Err(Failure::Cancelled)
    } else {
        Ok(())
    }
}

pub fn execute(mut batch: Batch, cancelled: impl Fn() -> bool) -> Result<Value, Failure> {
    for stage in batch.plan {
        check(&cancelled)?;
        match stage {
            Stage::Filter { predicate } => {
                let mut rows = Vec::new();
                for row in batch.rows {
                    check(&cancelled)?;
                    match predicate.value(&row, &batch.schema)? {
                        Value::Bool(true) => rows.push(row),
                        Value::Bool(false) => {}
                        _ => return Err(invalid("filter requires boolean")),
                    }
                }
                batch.rows = rows;
            }
            Stage::Aggregate { keys, aggregates } => {
                // BTreeMap indexes groups, while the vector preserves first appearance.
                let mut indices = BTreeMap::new();
                let mut groups: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
                for row in batch.rows {
                    check(&cancelled)?;
                    let key = keys
                        .iter()
                        .map(|e| e.value(&row, &batch.schema))
                        .collect::<Result<Vec<_>, _>>()?;
                    let identity =
                        serde_json::to_string(&key).map_err(|e| invalid(e.to_string()))?;
                    let index = *indices.entry(identity).or_insert_with(|| {
                        groups.push((key, Vec::new()));
                        groups.len() - 1
                    });
                    groups[index].1.push(row);
                }
                let mut rows = Vec::new();
                for (mut key, members) in groups {
                    for aggregate in &aggregates {
                        check(&cancelled)?;
                        key.push(Value::String(
                            aggregate
                                .aggregate(&members, &batch.schema, &cancelled)?
                                .to_string(),
                        ));
                    }
                    rows.push(Value::Array(key));
                }
                batch.rows = rows;
                batch.schema = keys
                    .iter()
                    .map(|e| e.name.clone())
                    .chain(aggregates.iter().map(|e| format!("{}:int64_text", e.name)))
                    .collect();
            }
            Stage::Sort { keys } => {
                // Validate all keys before sorting: the comparator is total even on
                // invalid input, and typed failures never escape as sorting panics.
                let mut decorated = Vec::new();
                for row in batch.rows {
                    check(&cancelled)?;
                    let values = keys
                        .iter()
                        .map(|key| key.sort_key(&row, &batch.schema))
                        .collect::<Result<Vec<_>, _>>()?;
                    if let Some((first, _)) = decorated.first() {
                        let first: &Vec<(SortValue, bool)> = first;
                        if first.iter().zip(&values).any(|(a, b)| {
                            std::mem::discriminant(&a.0) != std::mem::discriminant(&b.0)
                        }) {
                            return Err(invalid("mixed types in sort column"));
                        }
                    }
                    decorated.push((values, row));
                }
                decorated.sort_by(|(a, _), (b, _)| {
                    // Sorting must finish with a total comparator even if cancellation
                    // arrives here. Report it immediately after the bounded sort.
                    for ((a, ascending), (b, _)) in a.iter().zip(b) {
                        let order = a.cmp(b);
                        if order != Ordering::Equal {
                            return if *ascending { order } else { order.reverse() };
                        }
                    }
                    Ordering::Equal
                });
                check(&cancelled)?;
                batch.rows = decorated.into_iter().map(|(_, row)| row).collect();
            }
            Stage::Limit { n } => batch.rows.truncate(n),
        }
    }
    check(&cancelled)?;
    Ok(json!({"rows":batch.rows,"schema":batch.schema}))
}
