//! The cell vocabulary the scenarios are written in, and its two renderings.
//!
//! A cross-cell law is a statement about a *sequence of cells*, not about
//! either dialect's spelling of them, so the scenarios name cells with [`Cell`]
//! and this module renders one into Lashlang or TypeScript source. Two things
//! follow that are worth the indirection: a scenario written once runs against
//! both dialects, and the parity axis can compare them without a second corpus
//! that could drift from the first.
//!
//! Every variant also declares what it does to the session, via
//! [`SessionModel::apply`]. That is what lets the generative axis check a
//! generated sequence against an expectation instead of against itself.

use std::collections::BTreeMap;

use serde_json::json;

use super::harness::Dialect;

/// A value a cell can bind.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Literal {
    Number(f64),
    /// A list of numbers.
    List(Vec<f64>),
    /// A record of number-valued fields, plus one nested list field, so the
    /// corpus carries a shape with depth rather than only flat leaves.
    Nested {
        scalar: f64,
        items: Vec<f64>,
    },
}

impl Literal {
    fn render(&self, dialect: Dialect) -> String {
        match self {
            Self::Number(value) => render_number(*value),
            Self::List(items) => format!("[{}]", render_numbers(items)),
            Self::Nested { scalar, items } => match dialect {
                Dialect::Lashlang => format!(
                    "{{ \"scalar\": {}, \"items\": [{}] }}",
                    render_number(*scalar),
                    render_numbers(items)
                ),
                Dialect::Typescript => format!(
                    "{{ scalar: {}, items: [{}] }}",
                    render_number(*scalar),
                    render_numbers(items)
                ),
            },
        }
    }

    fn as_json(&self) -> serde_json::Value {
        match self {
            Self::Number(value) => number_json(*value),
            Self::List(items) => items.iter().copied().map(number_json).collect(),
            Self::Nested { scalar, items } => json!({
                "scalar": number_json(*scalar),
                "items": items.iter().copied().map(number_json).collect::<serde_json::Value>(),
            }),
        }
    }
}

/// The JSON shape an integral number takes in the session's exported view.
///
/// The projection writes an integral float as a JSON integer, so a model built
/// from `f64` literals would differ from the session on spelling alone and
/// every comparison would be about `serde_json` rather than about the session.
fn number_json(value: f64) -> serde_json::Value {
    json!(value as i64)
}

fn render_number(value: f64) -> String {
    // Both dialects read an integral float as the number it is, and the JSON
    // view compares equal to the same literal, so keep the spelling integral.
    format!("{}", value as i64)
}

fn render_numbers(items: &[f64]) -> String {
    items
        .iter()
        .map(|item| render_number(*item))
        .collect::<Vec<_>>()
        .join(", ")
}

/// One cell of a session, named by what it does rather than by its source.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Cell {
    /// Bind `name` to a literal, shadowing any earlier binding of that name.
    Bind { name: String, value: Literal },
    /// Bind `name` to one more than the number already bound to `source`.
    /// The cell that proves a later cell reads an earlier cell's work.
    Derive { name: String, source: String },
    /// Bind `name` to the list at `source` with `value` appended.
    ///
    /// The destination is a fresh name on purpose: TypeScript's `const` puts a
    /// same-cell redeclaration in its own temporal dead zone, so `xs = [...xs,
    /// v]` is not a thing the dialect can say. Growing a structure across cells
    /// therefore grows it into a new name, in both dialects, and shadowing gets
    /// its own cell shape ([`Cell::Bind`] over an existing name).
    Extend {
        name: String,
        source: String,
        value: f64,
    },
    /// Rebind `name` to null: the session keeps the name and drops the value.
    Drop { name: String },
    /// Finish the session with the value bound to `name`.
    Finish { name: String },
    /// A cell that allocates a closure which is unreachable by the time the
    /// cell ends, and binds `name` to a plain number computed with it.
    ///
    /// This is the FIG-1562 shape: the closure is garbage, but it is resident
    /// garbage, and validating the *next* cell's program against it is what
    /// poisoned the session.
    ClosureGarbage { name: String },
    /// A cell that binds `name` to a closure — a live root, not garbage.
    ///
    /// TypeScript only; see [`Cell::expressed_by`]. The ruled contract is that
    /// the binding does not survive the cell boundary, so the model drops it.
    ClosureBinding { name: String },
    /// A cell that does not compile.
    CompileError,
    /// A cell that compiles and fails at runtime, before it binds anything.
    RuntimeError,
    /// A cell the dialect refuses as outside the language, as distinct from a
    /// program that is merely wrong. TypeScript only.
    Refusal,
}

impl Cell {
    pub(crate) fn bind(name: &str, value: Literal) -> Self {
        Self::Bind {
            name: name.to_string(),
            value,
        }
    }

    pub(crate) fn number(name: &str, value: f64) -> Self {
        Self::bind(name, Literal::Number(value))
    }

    /// The destination must differ from the source: TypeScript's `const` puts a
    /// same-cell redeclaration in its own temporal dead zone, so a cell cannot
    /// read the name it is binding. Reading one name into another is the shape
    /// both dialects have.
    pub(crate) fn derive(name: &str, source: &str) -> Self {
        assert_ne!(name, source, "a derive cell cannot read the name it binds");
        Self::Derive {
            name: name.to_string(),
            source: source.to_string(),
        }
    }

    /// The destination must differ from the source; see [`Cell::derive`].
    pub(crate) fn extend(name: &str, source: &str, value: f64) -> Self {
        assert_ne!(name, source, "an extend cell cannot read the name it binds");
        Self::Extend {
            name: name.to_string(),
            source: source.to_string(),
            value,
        }
    }

    pub(crate) fn drop_value(name: &str) -> Self {
        Self::Drop {
            name: name.to_string(),
        }
    }

    pub(crate) fn finish(name: &str) -> Self {
        Self::Finish {
            name: name.to_string(),
        }
    }

    pub(crate) fn closure_garbage(name: &str) -> Self {
        Self::ClosureGarbage {
            name: name.to_string(),
        }
    }

    pub(crate) fn closure_binding(name: &str) -> Self {
        Self::ClosureBinding {
            name: name.to_string(),
        }
    }

    /// Whether `dialect` can express this cell at all.
    ///
    /// The two `false` answers are the whole content of the cross-dialect
    /// parity gaps, and [`super::parity`] states them as rows with reasons
    /// rather than leaving them implicit here.
    pub(crate) fn expressed_by(&self, dialect: Dialect) -> bool {
        match self {
            Self::ClosureBinding { .. } | Self::Refusal => dialect == Dialect::Typescript,
            _ => true,
        }
    }

    /// The source this cell takes in `dialect`.
    pub(crate) fn render(&self, dialect: Dialect) -> String {
        assert!(
            self.expressed_by(dialect),
            "{dialect} cannot express {self:?}"
        );
        match dialect {
            Dialect::Lashlang => self.render_lashlang(),
            Dialect::Typescript => self.render_typescript(),
        }
    }

    fn render_lashlang(&self) -> String {
        match self {
            Self::Bind { name, value } => format!("{name} = {}", value.render(Dialect::Lashlang)),
            Self::Derive { name, source } => format!("{name} = {source} + 1"),
            Self::Extend {
                name,
                source,
                value,
            } => format!("{name} = push({source}, {})", render_number(*value)),
            Self::Drop { name } => format!("{name} = null"),
            Self::Finish { name } => format!("finish {name}"),
            // A declared `fn` is materialized at its call site as a
            // capture-free closure over the chunk function, so this cell leaves
            // a closure on the heap with nothing rooting it — the same
            // condition an inline arrow leaves in TypeScript.
            Self::ClosureGarbage { name } => {
                format!("fn cell_scale(n: float) -> float {{ n * 2 }}\n{name} = cell_scale(3)")
            }
            Self::CompileError => "finish no_such_name".to_string(),
            // `len` of a number fails at runtime, and it fails before the cell
            // has bound anything, so a failed cell leaves no partial state for
            // the no-poisoning law to have to excuse.
            Self::RuntimeError => "finish len(3)".to_string(),
            Self::ClosureBinding { .. } | Self::Refusal => unreachable!("guarded above"),
        }
    }

    fn render_typescript(&self) -> String {
        match self {
            Self::Bind { name, value } => {
                format!("const {name} = {};", value.render(Dialect::Typescript))
            }
            Self::Derive { name, source } => format!("const {name} = {source} + 1;"),
            Self::Extend {
                name,
                source,
                value,
            } => format!("const {name} = [...{source}, {}];", render_number(*value)),
            Self::Drop { name } => format!("const {name} = null;"),
            Self::Finish { name } => format!("finish({name});"),
            Self::ClosureGarbage { name } => {
                format!("const {name} = [1, 2, 3].map(value => value * 2)[2];")
            }
            Self::ClosureBinding { name } => {
                format!("const {name} = (value: number) => value + 1;")
            }
            Self::CompileError => "finish(noSuchName);".to_string(),
            Self::RuntimeError => "throw new Error(\"cell failed\");".to_string(),
            Self::Refusal => "class NotADialectConstruct {}".to_string(),
        }
    }
}

/// What a cell is required to do.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Expectation {
    Succeeds {
        /// The terminal value the cell finishes the session with, which is
        /// `None` for every cell that is not a finish.
        ///
        /// Carrying it here rather than leaving it to the scenarios is what
        /// makes the generative axis able to see a terminal-value regression:
        /// thousands of generated sessions end in a finish over a name an
        /// earlier cell bound, and a session that returned the wrong number
        /// there would otherwise pass every one of them.
        finish: Option<serde_json::Value>,
    },
    /// The cell fails, and says so with a typed error. Nothing else about the
    /// session may change, and the session does not finish.
    FailsTyped,
}

impl Expectation {
    fn succeeds() -> Self {
        Self::Succeeds { finish: None }
    }
}

/// The bindings a session is required to hold, tracked cell by cell.
///
/// This is the oracle the generative axis checks against: without it a
/// generated sequence can only be compared to itself, which is exactly the
/// weakness that let FIG-1562 through — every ingredient was asserted, no
/// composition was.
#[derive(Clone, Debug, Default)]
pub(crate) struct SessionModel {
    bindings: BTreeMap<String, serde_json::Value>,
}

impl SessionModel {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn bindings(&self) -> &BTreeMap<String, serde_json::Value> {
        &self.bindings
    }

    pub(crate) fn get(&self, name: &str) -> Option<&serde_json::Value> {
        self.bindings.get(name)
    }

    /// Advances the model over `cell` and returns what the cell must do.
    pub(crate) fn apply(&mut self, cell: &Cell) -> Expectation {
        match cell {
            Cell::Bind { name, value } => {
                self.bindings.insert(name.clone(), value.as_json());
                Expectation::succeeds()
            }
            Cell::Derive { name, source } => {
                let base = self
                    .bindings
                    .get(source)
                    .and_then(serde_json::Value::as_f64)
                    .expect("a derive cell reads a bound number");
                self.bindings.insert(name.clone(), number_json(base + 1.0));
                Expectation::succeeds()
            }
            Cell::Extend {
                name,
                source,
                value,
            } => {
                let mut list = self
                    .bindings
                    .get(source)
                    .and_then(serde_json::Value::as_array)
                    .expect("an extend cell reads a bound list")
                    .clone();
                list.push(number_json(*value));
                self.bindings
                    .insert(name.clone(), serde_json::Value::Array(list));
                Expectation::succeeds()
            }
            Cell::Drop { name } => {
                self.bindings.insert(name.clone(), serde_json::Value::Null);
                Expectation::succeeds()
            }
            Cell::Finish { name } => {
                let value = self
                    .bindings
                    .get(name)
                    .expect("a finish cell reads a bound name")
                    .clone();
                Expectation::Succeeds {
                    finish: Some(value),
                }
            }
            Cell::ClosureGarbage { name } => {
                self.bindings.insert(name.clone(), number_json(6.0));
                Expectation::succeeds()
            }
            // The ruled contract, stated as a model transition: a closure-valued
            // binding does not reach the next cell, so the session holds no
            // binding of that name afterwards. See
            // `docs/adr/0059-lashlang-durable-stores-hold-exclusively-owned-copies.md`
            // (three ADRs carry the number 0059; this is the one).
            Cell::ClosureBinding { name } => {
                self.bindings.remove(name);
                Expectation::succeeds()
            }
            Cell::CompileError | Cell::RuntimeError | Cell::Refusal => Expectation::FailsTyped,
        }
    }
}
