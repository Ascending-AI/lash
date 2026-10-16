//! Host-neutral construction of canonical edit transactions.
//!
//! [`plan`] lowers convenience operations into ordinary [`Edit`] values. It
//! neither mutates nor admits the draft. Apply its result once with
//! [`Draft::apply`] to commit the complete batch or refuse it unchanged.
//!
//! Every position addresses the base document, including direct edits between
//! helpers. Generated statements become targetable by site after admission,
//! in the next transaction. Within this batch, later helpers can refer to an
//! earlier helper's explicitly named output with [`Expr::Variable`]. An
//! operation writes one statement per operation: computed inputs stay where
//! the author wrote them, as arguments of a nested action (`K-EVAL-008`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Draft, Edit, Position, Transaction};
use lash_kernel_doc::{ActExpr, ActKind, EffectName, Expr, Name, Rhs, Stmt, Type};

/// One host authoring operation, lowered by [`plan`] to canonical edits.
///
/// Lash owns this operation data and its serialized representation. Hosts own
/// their transport, revision and authentication envelope, embedding batches as
/// `Vec<Operation>`. Save the expanded [`Transaction`] as the exact edit record
/// to replay against its base and admission context.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub enum Operation {
    /// The complete existing edit vocabulary, with base-document targets.
    #[serde(rename = "edit")]
    Edit(Edit),
    /// Insert a named effect result, its inputs written as its arguments.
    ///
    /// `binding` is the caller's name, preserved verbatim. Ordinary admission
    /// decides whether the name, scope, effect and result type are valid.
    #[serde(rename = "insert_effect")]
    InsertEffect {
        /// Insertion position in the transaction's base document.
        at: Position,
        /// The effect to perform, checked against the admission environment.
        effect: EffectName,
        /// Arguments in evaluation order, any expressions.
        args: Vec<Expr>,
        /// The result type stated by the canonical `perform` action.
        result: Type,
        /// Caller-selected output name. It is preserved, not made fresh.
        binding: Name,
    },
}

/// Build one transaction without executing effects or changing the draft.
///
/// Operations lower in their supplied order, each to one edit; the same
/// draft and operations produce the same transaction. An effect inserts one
/// `let` of the caller's name whose right-hand side performs the effect on
/// the given arguments: the statement's action when every argument is a
/// variable or a literal, else that action nested as the whole right-hand
/// side, its computed arguments in place.
///
/// All targets address the base document. A later operation can reference an
/// earlier effect's named output when lexical scope permits, but newly inserted
/// statements have targetable sites only after applying this transaction.
/// Planning performs no admission: [`Draft::apply`] checks targets, scope and
/// types once for the complete transaction and refuses it without mutation.
/// Its diagnostic indices name the edits, one per operation.
#[must_use]
pub fn plan(draft: &Draft, operations: impl IntoIterator<Item = Operation>) -> Transaction {
    let edits = operations
        .into_iter()
        .map(|operation| match operation {
            Operation::Edit(edit) => edit,
            Operation::InsertEffect {
                at,
                effect,
                args,
                result,
                binding,
            } => {
                let act = ActExpr {
                    kind: ActKind::Perform {
                        effect,
                        result,
                        then_yield: false,
                    },
                    args,
                };
                Edit::InsertStatement {
                    at,
                    statement: Stmt::Let {
                        name: binding,
                        value: match act.as_action() {
                            Some(action) => Rhs::Action(action),
                            None => Rhs::Expr(Expr::Act(Box::new(act))),
                        },
                    },
                }
            }
        })
        .collect();
    Transaction {
        base: draft.identity(),
        edits,
    }
}

#[cfg(test)]
mod tests {
    use lash_kernel_check::Environment;
    use lash_kernel_doc::{
        Action, Atom, Document, DocumentId, FunctionRegistry, Literal, NumberPolicy, Param,
        Signature, Site, Unit,
    };

    use super::*;

    fn variable(name: &str) -> Expr {
        Expr::Variable(Name::new(name))
    }

    fn binding(name: &str) -> Stmt {
        Stmt::Let {
            name: Name::new(name),
            value: Rhs::Expr(Expr::Literal(Literal::Text(name.to_owned()))),
        }
    }

    /// The schema positions that admit any JSON value at all.
    fn open_positions(schema: &serde_json::Value, path: &str, out: &mut Vec<String>) {
        use serde_json::Value;
        match schema {
            Value::Bool(true) => out.push(path.to_owned()),
            Value::Object(object) if object.is_empty() => out.push(path.to_owned()),
            Value::Object(object) => {
                for (key, value) in object {
                    let path = format!("{path}/{key}");
                    match (key.as_str(), value) {
                        ("properties" | "$defs", Value::Object(members)) => {
                            for (member, schema) in members {
                                open_positions(schema, &format!("{path}/{member}"), out);
                            }
                        }
                        ("oneOf" | "anyOf" | "allOf" | "prefixItems", Value::Array(schemas)) => {
                            for (index, schema) in schemas.iter().enumerate() {
                                open_positions(schema, &format!("{path}/{index}"), out);
                            }
                        }
                        ("items" | "additionalProperties", schema)
                            if *schema != Value::Bool(false) =>
                        {
                            open_positions(schema, &path, out);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    /// EDIT-CLOSED: the authoring wire carries document edits only. Host
    /// labels and node data are not edits: their spellings refuse to decode
    /// and no schema position admits open JSON. The expanded transaction is
    /// the exact replay record: decoded and applied to a fresh draft on its
    /// base, it publishes the same document and correspondence.
    #[test]
    fn edit_wire_is_closed_and_transactions_replay_exactly() {
        let node = serde_json::json!({"unit": "main", "path": [0]});
        serde_json::from_value::<Edit>(
            serde_json::json!({"remove_statement": {"statement": node}}),
        )
        .unwrap();
        for retired in [
            serde_json::json!({"set_label": {"node": node, "label": {"title": "host"}}}),
            serde_json::json!({"set_data": {"node": node, "key": "host", "value": {"x": 9}}}),
        ] {
            assert!(serde_json::from_value::<Edit>(retired.clone()).is_err());
            assert!(
                serde_json::from_value::<Operation>(serde_json::json!({"edit": retired.clone()}))
                    .is_err()
            );
            let transaction = serde_json::json!({
                "base": DocumentId::from_bytes([0; 32]).to_string(),
                "edits": [retired],
            });
            assert!(Transaction::from_json(&transaction.to_string()).is_err());
        }
        for schema in [
            serde_json::to_value(schemars::schema_for!(Operation)).unwrap(),
            serde_json::to_value(schemars::schema_for!(Transaction)).unwrap(),
        ] {
            let mut open = Vec::new();
            open_positions(&schema, "", &mut open);
            assert_eq!(open, Vec::<String>::new());
            let text = schema.to_string();
            assert!(!text.contains("set_label") && !text.contains("set_data"));
        }

        let base = Document::new(
            NumberPolicy::Float,
            vec![
                binding("input"),
                Stmt::Print {
                    value: variable("input"),
                },
            ],
        );
        let functions = FunctionRegistry::default();
        let mut environment = Environment::new(&functions);
        let effect = EffectName::new("mock.collect").unwrap();
        environment.effects.insert(
            effect.clone(),
            Signature {
                params: vec![Param {
                    name: Name::new("items"),
                    ty: Type::Any,
                    optional: false,
                }],
                result: Type::Any,
            },
        );
        let mut first = Draft::open(base.clone()).unwrap();
        let print = Site::new(Unit::Main, [1]);
        let operations = vec![
            Operation::InsertEffect {
                at: Position::before(print.clone()).unwrap(),
                effect,
                args: vec![Expr::List(vec![variable("input")])],
                result: Type::Any,
                binding: Name::new("output"),
            },
            Operation::Edit(Edit::CloneStatement {
                statement: print.clone(),
                to: Position::end(Site::new(Unit::Main, [])),
            }),
        ];
        let encoded = serde_json::to_value(&operations).unwrap();
        for (index, variant) in [(0, "insert_effect"), (1, "edit")] {
            let mut extra = encoded[index].clone();
            extra[variant]["unknown"] = serde_json::json!(true);
            assert!(serde_json::from_value::<Operation>(extra).is_err());
        }
        let decoded: Vec<Operation> = serde_json::from_value(encoded).unwrap();
        let transaction = plan(&first, operations);
        assert_eq!(plan(&first, decoded), transaction);
        let applied = first.apply(&transaction, &environment).unwrap();

        let replayed = Transaction::from_json(&transaction.to_json().unwrap()).unwrap();
        assert_eq!(replayed, transaction);
        let mut second = Draft::open(base).unwrap();
        let again = second.apply(&replayed, &environment).unwrap();
        assert_eq!(second.document(), first.document());
        assert_eq!(again, applied);
        // The clone is new: only the original print survives from the base.
        assert_eq!(
            applied.correspondence.successor(&print),
            Some(&Site::new(Unit::Main, [2]))
        );
        assert_eq!(
            applied
                .correspondence
                .predecessor(&Site::new(Unit::Main, [3])),
            None
        );
    }

    /// K-EVAL-008: an inserted effect is one statement. A computed input
    /// stays the argument the host wrote, in a nested `perform` that is the
    /// whole right-hand side; inputs that are all variables and literals
    /// make the statement's own action. No temporary is written.
    #[test]
    fn an_inserted_effect_keeps_computed_inputs_as_its_arguments() {
        let mut draft = Draft::open(Document::new(
            NumberPolicy::Float,
            vec![
                binding("input"),
                Stmt::Print {
                    value: variable("input"),
                },
            ],
        ))
        .unwrap();
        let functions = FunctionRegistry::default();
        let mut environment = Environment::new(&functions);
        let effect = EffectName::new("mock.collect").unwrap();
        environment.effects.insert(
            effect.clone(),
            Signature {
                params: vec![Param {
                    name: Name::new("items"),
                    ty: Type::Any,
                    optional: false,
                }],
                result: Type::Any,
            },
        );
        let anchor = Site::new(Unit::Main, [1]);
        let at = Position::before(anchor.clone()).unwrap();
        let computed = Expr::List(vec![variable("input")]);
        let transaction = plan(
            &draft,
            [
                Operation::InsertEffect {
                    at: at.clone(),
                    effect: effect.clone(),
                    args: vec![computed.clone()],
                    result: Type::Any,
                    binding: Name::new("first"),
                },
                Operation::InsertEffect {
                    at,
                    effect: effect.clone(),
                    args: vec![variable("first")],
                    result: Type::Any,
                    binding: Name::new("second"),
                },
            ],
        );
        assert_eq!(transaction.edits.len(), 2);
        let applied = draft.apply(&transaction, &environment).unwrap();
        assert_eq!(
            draft.document().main[1..3],
            [
                Stmt::Let {
                    name: Name::new("first"),
                    value: Rhs::Expr(Expr::Act(Box::new(ActExpr {
                        kind: ActKind::Perform {
                            effect: effect.clone(),
                            result: Type::Any,
                            then_yield: false,
                        },
                        args: vec![computed],
                    }))),
                },
                Stmt::Let {
                    name: Name::new("second"),
                    value: Rhs::Action(Action::Perform {
                        effect,
                        args: vec![Atom::Variable(Name::new("first"))],
                        result: Type::Any,
                        then_yield: false,
                    }),
                },
            ]
        );
        assert_eq!(
            applied.correspondence.successor(&anchor),
            Some(&Site::new(Unit::Main, [3]))
        );
    }
}
