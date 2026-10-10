//! Functions a session keeps between cells.
//!
//! A closure never outlives its run (`K-SES-003`). What a session keeps of
//! a function a cell bound is a [`SavedFunction`]: the closure's code as a
//! declared function of a document of its own, and the variables it read
//! from outside itself, frozen as data when its cell ended. Nothing in it
//! names the cell or the session it came from.
//!
//! A later cell calls one by having it copied into its own document
//! ([`install`]): the function is declared there under its name, with each
//! capture bound to its constant ahead of the body, and the cell holds a
//! reference to that declaration. The machine runs one document, as ever.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{
    Action, Annotations, Atom, Callee, Datum, Document, EffectName, Expr, Function, FunctionId,
    Literal, MapEntry, Member, Name, Node, Object, ObjectId, Place, RecordEntry, Rhs, Signature,
    Site, Stmt, Unit, Value, ValueKind,
};
use serde::{Deserialize, Serialize};

use crate::FunctionValues;

/// A function a session holds as a value of its own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedFunction {
    /// The name `document` declares the function under: the binding it was
    /// saved from, the first by name of those that held it.
    pub name: Name,
    /// The function's code: a document that declares it and nothing else,
    /// with an empty `main`. Its manifest is what the function requires of
    /// an environment: the effects it performs and the library functions
    /// it calls. The body opens with the functions it held that no binding
    /// named, and is free in the names of `captures`.
    pub document: Document,
    /// What the function read from outside itself, as it stood when its
    /// cell ended. A `Datum::Function` in one names another saved
    /// function, which is found by that name where this one is used.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub captures: BTreeMap<Name, Datum>,
    /// What the function's dialect stated of it, which the kernel reads
    /// none of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written: Option<Written>,
    /// The token the binding held the function in, where its dialect holds
    /// a function with data about it ([`FunctionValues::Token`]), as it
    /// stood when its cell ended. A cell that uses the function binds its
    /// name to this ([`SavedFunction::value`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<Token>,
}

/// A dialect's function token around a saved function, frozen: a tuple of
/// `tag`, the members `before`, the function, and the members `after`. The
/// function is structural: it is always the one the token is saved with,
/// and every other member is constant data about it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Token {
    pub tag: String,
    pub before: Vec<Constant>,
    pub after: Vec<Constant>,
}

impl Token {
    /// The token around `value`'s one function member, `value` being a
    /// tuple whose first member is `tag`, with every other member frozen
    /// from `objects`; `None` when it is no such token or a member is not
    /// constant data.
    fn frozen(
        value: &Value,
        tag: &str,
        objects: &BTreeMap<ObjectId, Object>,
        named: &BTreeMap<ObjectId, Name>,
        callable: impl Fn(&Value) -> bool,
    ) -> Option<Self> {
        let Value::Tuple(members) = value else {
            return None;
        };
        let (first, rest) = members.split_first()?;
        if !matches!(first, Value::Text(first) if &**first == tag) {
            return None;
        }
        let mut at = rest
            .iter()
            .enumerate()
            .filter(|(_, member)| callable(member))
            .map(|(at, _)| at);
        let (Some(at), None) = (at.next(), at.next()) else {
            return None;
        };
        let each = |members: &[Value]| {
            members
                .iter()
                .map(|member| {
                    let datum = freeze(member, objects, named, &mut Vec::new()).ok()?;
                    Constant::try_from(datum).ok()
                })
                .collect::<Option<Vec<_>>>()
        };
        Some(Self {
            tag: tag.to_owned(),
            before: each(&rest[..at])?,
            after: each(&rest[at + 1..])?,
        })
    }

    /// The token as a constant expression around `function`.
    fn expr(&self, function: Expr) -> Expr {
        let mut members = Vec::with_capacity(self.before.len() + self.after.len() + 2);
        members.push(Expr::Literal(Literal::Text(self.tag.clone())));
        members.extend(self.before.iter().map(Constant::expr));
        members.push(function);
        members.extend(self.after.iter().map(Constant::expr));
        Expr::Tuple(members)
    }
}

/// Data a cell writes as a constant expression: a datum that holds no
/// function reference, undecoded number, timestamp, error or handle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Datum", into = "Datum")]
pub struct Constant(Datum);

impl TryFrom<Datum> for Constant {
    type Error = &'static str;

    fn try_from(datum: Datum) -> Result<Self, Self::Error> {
        fn constant(datum: &Datum) -> bool {
            match datum {
                Datum::Null
                | Datum::Absent
                | Datum::Bool(_)
                | Datum::Int(_)
                | Datum::Float(_)
                | Datum::Text(_)
                | Datum::Bytes(_) => true,
                Datum::Tuple(items) | Datum::List(items) | Datum::Set(items) => {
                    items.iter().all(constant)
                }
                Datum::Map(entries) => entries
                    .iter()
                    .all(|(key, value)| constant(key) && constant(value)),
                Datum::Record(fields) => fields.iter().all(|(_, value)| constant(value)),
                Datum::Number(_)
                | Datum::Timestamp(_)
                | Datum::Error(_)
                | Datum::Function(_)
                | Datum::Handle(_) => false,
            }
        }
        if constant(&datum) {
            Ok(Self(datum))
        } else {
            Err("a token's data about its function is constant data, without a function")
        }
    }
}

impl From<Constant> for Datum {
    fn from(constant: Constant) -> Self {
        constant.0
    }
}

impl Constant {
    fn expr(&self) -> Expr {
        // Construction admits only data `constant` writes.
        constant(&self.0).unwrap_or(Expr::Literal(Literal::Absent))
    }
}

/// A function a cell left, saved once however many of its bindings held
/// it.
#[derive(Clone, Debug, PartialEq)]
pub struct Kept {
    pub function: SavedFunction,
    /// The other bindings that held the function, besides the one it is
    /// declared under ([`SavedFunction::name`]).
    pub aliases: BTreeSet<Name>,
}

/// A saved function as its dialect wrote it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Written {
    pub dialect: String,
    /// The function's signature in the dialect's own type syntax.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// The source text of the statement that bound it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The signature a host starts the function under, where every
    /// parameter is one a start can name and type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<Signature>,
    /// Dialect-owned call metadata, interpreted only by that dialect's lowerer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Why a binding that holds a function was not saved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NotSaved {
    /// The binding reaches a task: work that is still running, or its
    /// handle.
    #[error("it holds a task or a pending promise, which ends with its cell")]
    Task,
    /// The binding is data that holds a function somewhere inside it.
    #[error(
        "it holds a function inside other data; only a function bound to a name of its own is kept"
    )]
    FunctionInData,
    /// The function was made inside a library function's body.
    #[error("its function was made by a library function, whose code is not the cell's")]
    MadeByLibrary,
    /// A variable the function reads cannot be frozen.
    #[error("its function reads `{name}`, {why}")]
    Capture { name: Name, why: CaptureRefusal },
    /// The function calls another by name that was itself not saved.
    #[error("its function uses `{function}`, which is not a saved function")]
    Needs { function: Name },
    /// The token the binding held its function in holds data about it that
    /// is not constant ([`Constant`]).
    #[error("its function's value holds data about the function that is no constant")]
    Token,
    /// The function performs an effect whose call ends the turn. Only a
    /// cell's `main` ends its turn, so no function is kept that would.
    #[error(
        "its function calls `{effect}`, which ends the turn; return the value and call `{effect}` from the cell's top level"
    )]
    PerformsControl { effect: EffectName },
    /// The session's dialect declares no saved function in a later cell.
    #[error("it holds a function, and this session's dialect keeps none between cells")]
    Dialect,
    /// The function was saved under an earlier kernel version, whose
    /// migration does not carry it to this one.
    #[error(
        "its function was saved under kernel version {from} and is not carried forward: {problem}"
    )]
    NotMigrated { from: u32, problem: String },
    /// The run's end did not describe the closure as its document has it.
    #[error("its function cannot be read back from the cell's document: {problem}")]
    Unreadable { problem: String },
}

/// Why one captured variable cannot be frozen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CaptureRefusal {
    #[error("which holds a task or a pending promise")]
    Task,
    #[error("which holds a function that is bound to no name of its own")]
    Function,
    #[error("which holds data that contains itself")]
    Cyclic,
    /// Two of the functions it holds read different variables of this
    /// name.
    #[error("which names two different variables among the functions it holds")]
    Conflict,
    #[error("which holds a {kind:?} value, and no constant is written for one")]
    NoConstant { kind: ValueKind },
}

/// What a run's end left: the cell's document, the bindings that were
/// carried, and the ones that were not because they reach a closure.
#[derive(Clone, Copy)]
pub struct Left<'a> {
    pub document: &'a Document,
    pub carried: &'a BTreeMap<Name, Value>,
    pub carried_objects: &'a BTreeMap<ObjectId, Object>,
    pub closures: &'a BTreeMap<Name, Value>,
    pub closure_objects: &'a BTreeMap<ObjectId, Object>,
    /// Every binding that was not carried, those that reach a task among
    /// them.
    pub not_carried: &'a [Name],
    /// The effects whose call ends the turn
    /// ([`crate::Environment::controls`]), as the cell was lowered against
    /// them.
    pub controls: &'a BTreeMap<EffectName, BTreeSet<crate::EffectControl>>,
    /// The document's annotations, which state what each function's
    /// source wrote of it ([`WRITTEN`]).
    pub annotations: Option<&'a Annotations>,
    /// What the session's dialect holds a function value in. A binding
    /// that holds a token is saved as the closure it holds, with the token.
    pub values: &'a FunctionValues,
}

/// The closure a dialect's function token holds, and its position: `value`
/// is a tuple whose first member is `tag` and that holds exactly one
/// closure.
fn token_closure(value: &Value, tag: Option<&str>) -> Option<(ObjectId, usize)> {
    let (tag, Value::Tuple(members)) = (tag?, value) else {
        return None;
    };
    if !matches!(members.first(), Some(Value::Text(first)) if &**first == tag) {
        return None;
    }
    let mut closures = members
        .iter()
        .enumerate()
        .filter_map(|(at, member)| match member {
            Value::Closure(id) => Some((*id, at)),
            _ => None,
        });
    let closure = closures.next()?;
    closures.next().is_none().then_some(closure)
}

/// The closure a binding's value is: the closure itself, or the one a
/// dialect's function token holds.
fn closure_held(value: &Value, tag: Option<&str>) -> Option<ObjectId> {
    match value {
        Value::Closure(id) => Some(*id),
        _ => token_closure(value, tag).map(|(id, _)| id),
    }
}

/// The saved function a binding's value refers to: a reference to a
/// declared function, or a dialect's function token around one.
pub fn function_reference<'v>(value: &'v Value, values: &FunctionValues) -> Option<&'v Name> {
    match value {
        Value::Function(name) => Some(name),
        Value::Tuple(members) => {
            let tag = values.tag()?;
            if !matches!(members.first(), Some(Value::Text(first)) if &**first == tag) {
                return None;
            }
            let mut references = members.iter().filter_map(|member| match member {
                Value::Function(name) => Some(name),
                _ => None,
            });
            let reference = references.next()?;
            references.next().is_none().then_some(reference)
        }
        _ => None,
    }
}

/// A dialect's function token around a reference to a saved function,
/// frozen ([`Token`]), or `None` for a bare reference or a token holding
/// something that is no constant.
pub fn token_of(
    value: &Value,
    objects: &BTreeMap<ObjectId, Object>,
    values: &FunctionValues,
) -> Option<Token> {
    Token::frozen(value, values.tag()?, objects, &BTreeMap::new(), |member| {
        matches!(member, Value::Function(_))
    })
}

/// The annotation key under which a dialect states, on the statement that
/// binds a function, what the source wrote of it: an object with the
/// function's `signature` as text and, where a host may start it, the
/// kernel signature it is started under as `start`.
pub const WRITTEN: &str = "function";

/// The annotation key of a statement's byte span in the source: `[start,
/// end]`.
const SPAN: &str = "span";

/// What the annotations state of the function the closure at `site` is.
fn written(annotations: &Annotations, site: &Site) -> Option<Written> {
    // The closure is the right-hand side of the statement that binds it.
    let (_, statement) = site.path.split_last()?;
    let node = annotations
        .nodes
        .iter()
        .find(|node| node.site.unit == site.unit && node.site.path == statement)?;
    let stated = node.data.get(WRITTEN);
    let source = node.data.get(SPAN).and_then(|span| {
        let start = usize::try_from(span.get(0)?.as_u64()?).ok()?;
        let end = usize::try_from(span.get(1)?.as_u64()?).ok()?;
        annotations.source.as_deref()?.get(start..end)
    });
    Some(Written {
        dialect: annotations.dialect.clone()?,
        signature: stated
            .and_then(|stated| stated.get("signature"))
            .and_then(|signature| signature.as_str())
            .map(str::to_owned),
        source: source.map(str::to_owned),
        metadata: stated.and_then(|stated| stated.get("metadata")).cloned(),
        start: stated
            .and_then(|stated| stated.get("start"))
            .and_then(|start| serde_json::from_value(start.clone()).ok()),
    })
}

/// Saves every function of `left.closures` whose captures are data or
/// other saved functions, once however many bindings hold it, and says of
/// each other binding why it was not saved. `held` names the functions the
/// session already holds, which a capture may name.
pub fn save(
    left: Left<'_>,
    held: &BTreeSet<Name>,
) -> (BTreeMap<Name, Kept>, BTreeMap<Name, NotSaved>) {
    let tag = left.values.tag();
    // A closure a binding holds directly, or in its dialect's function
    // token, is known by the first binding by name that holds it.
    let mut named: BTreeMap<ObjectId, Name> = BTreeMap::new();
    for (name, value) in left.closures {
        if let Some(id) = closure_held(value, tag) {
            named.entry(id).or_insert_with(|| name.clone());
        }
    }
    let mut saved: BTreeMap<Name, Kept> = BTreeMap::new();
    let mut refused = BTreeMap::new();
    let mut aliases: BTreeMap<Name, BTreeSet<Name>> = BTreeMap::new();
    for name in left.not_carried {
        let Some(value) = left.closures.get(name) else {
            refused.insert(name.clone(), NotSaved::Task);
            continue;
        };
        let Some((id, known)) = closure_held(value, tag).and_then(|id| Some((id, named.get(&id)?)))
        else {
            refused.insert(name.clone(), NotSaved::FunctionInData);
            continue;
        };
        // Another binding of the function is the one it is saved under.
        if known != name {
            aliases
                .entry(known.clone())
                .or_default()
                .insert(name.clone());
            continue;
        }
        let kept = save_closure(&left, name, id, &named).and_then(|mut function| {
            function.token = frozen_token(&left, value, &named)?;
            Ok(function)
        });
        match kept {
            Ok(function) => {
                saved.insert(
                    name.clone(),
                    Kept {
                        function,
                        aliases: BTreeSet::new(),
                    },
                );
            }
            Err(why) => {
                refused.insert(name.clone(), why);
            }
        }
    }
    for (name, names) in aliases {
        if let Some(kept) = saved.get_mut(&name) {
            kept.aliases = names;
        } else if let Some(why) = refused.get(&name).cloned() {
            refused.extend(names.into_iter().map(|alias| (alias, why.clone())));
        }
    }
    // A function that names one that was not saved is not saved either.
    loop {
        let missing = saved.iter().find_map(|(name, Kept { function, .. })| {
            function
                .needs()
                .into_iter()
                .find(|needed| {
                    // Still held is one the cell left bound to itself.
                    let still_held = held.contains(needed)
                        && left
                            .carried
                            .get(needed)
                            .and_then(|value| function_reference(value, left.values))
                            == Some(needed);
                    !saved.contains_key(needed) && !still_held
                })
                .map(|needed| (name.clone(), needed))
        });
        let Some((name, function)) = missing else {
            break;
        };
        if let Some(kept) = saved.remove(&name) {
            for alias in kept.aliases {
                refused.insert(
                    alias,
                    NotSaved::Needs {
                        function: function.clone(),
                    },
                );
            }
        }
        refused.insert(name, NotSaved::Needs { function });
    }
    (saved, refused)
}

/// The token a binding held its function in, frozen; `None` for a bare
/// closure.
fn frozen_token(
    left: &Left<'_>,
    value: &Value,
    named: &BTreeMap<ObjectId, Name>,
) -> Result<Option<Token>, NotSaved> {
    let Some(tag) = left.values.tag() else {
        return Ok(None);
    };
    if token_closure(value, Some(tag)).is_none() {
        return Ok(None);
    }
    Token::frozen(value, tag, left.closure_objects, named, |member| {
        matches!(member, Value::Closure(_))
    })
    .map(Some)
    .ok_or(NotSaved::Token)
}

fn save_closure(
    left: &Left<'_>,
    name: &Name,
    id: ObjectId,
    named: &BTreeMap<ObjectId, Name>,
) -> Result<SavedFunction, NotSaved> {
    let unreadable = |problem: &str| NotSaved::Unreadable {
        problem: problem.to_owned(),
    };
    let Some(Object::Closure(object)) = left.closure_objects.get(&id) else {
        return Err(unreadable("the binding names no closure"));
    };
    let mut frozen = Frozen {
        left,
        named,
        reads: Reads::default(),
        bound: BTreeMap::new(),
        helpers: Vec::new(),
    };
    let closure = frozen.absorb(object)?;
    let Frozen {
        reads,
        bound,
        helpers,
        ..
    } = frozen;

    let mut document = Document::new(left.document.manifest.numbers, Vec::new());
    if let Some(effect) = reads
        .effects
        .iter()
        .find(|effect| left.controls.contains_key(*effect))
    {
        return Err(NotSaved::PerformsControl {
            effect: effect.clone(),
        });
    }
    for effect in &reads.effects {
        let signature = left
            .document
            .manifest
            .effects
            .get(effect)
            .ok_or_else(|| unreadable("it performs an effect its document does not list"))?;
        document
            .manifest
            .effects
            .insert(effect.clone(), signature.clone());
    }
    for function in &reads.library {
        let listed = left
            .document
            .manifest
            .functions
            .get(function)
            .ok_or_else(|| unreadable("it calls a function its document does not list"))?;
        document
            .manifest
            .functions
            .insert(*function, listed.clone());
    }
    // The functions it holds that no binding names are part of its code.
    // Each is declared before any is made, so one that reads another, or
    // itself, shares that variable.
    let mut body = Vec::with_capacity(helpers.len() * 2 + closure.body.len());
    for helper in &helpers {
        body.push(Stmt::Let {
            name: helper.name.clone(),
            value: Rhs::Expr(Expr::Literal(Literal::Absent)),
        });
    }
    for helper in helpers {
        let made = Expr::Closure(Box::new(helper.code));
        let value = match helper.token {
            Some((mut members, at)) => {
                members[at] = made;
                Expr::Tuple(members)
            }
            None => made,
        };
        body.push(Stmt::Assign {
            place: Place::Variable(helper.name),
            value: Rhs::Expr(value),
        });
    }
    body.extend(closure.body.iter().cloned());
    document.functions.insert(
        name.clone(),
        Function {
            params: closure.params.clone(),
            body,
        },
    );
    Ok(SavedFunction {
        name: name.clone(),
        document,
        captures: bound
            .into_iter()
            .filter_map(|(name, held)| match held {
                Held::Data(datum) => Some((name, datum)),
                Held::Helper(_) => None,
            })
            .collect(),
        written: left
            .annotations
            .and_then(|annotations| written(annotations, &object.site)),
        token: None,
    })
}

/// What a name a saved function reads stands for.
enum Held {
    /// A value, frozen.
    Data(Datum),
    /// A closure no binding names, by the object it was.
    Helper(ObjectId),
}

/// A closure being frozen, with every closure it holds that no binding
/// names.
struct Frozen<'a> {
    left: &'a Left<'a>,
    named: &'a BTreeMap<ObjectId, Name>,
    reads: Reads,
    bound: BTreeMap<Name, Held>,
    helpers: Vec<Helper>,
}

/// A closure no binding names, made again in the saved function's code.
struct Helper {
    name: Name,
    code: lash_kernel_doc::Closure,
    /// The dialect's function token it was held in: the token's members
    /// as constants, and the position of the closure among them.
    token: Option<(Vec<Expr>, usize)>,
}

impl<'a> Frozen<'a> {
    /// Reads the closure `object` is from its document and freezes every
    /// variable it reads from outside itself.
    fn absorb(
        &mut self,
        object: &lash_kernel_doc::ClosureObject,
    ) -> Result<&'a lash_kernel_doc::Closure, NotSaved> {
        let left = self.left;
        let unreadable = |problem: &str| NotSaved::Unreadable {
            problem: problem.to_owned(),
        };
        let body = match &object.site.unit {
            Unit::Main => &left.document.main,
            Unit::Function(function) => left
                .document
                .functions
                .get(function)
                .map(|function| &function.body)
                .ok_or_else(|| unreadable("its unit is not declared"))?,
            Unit::Library(_) => return Err(NotSaved::MadeByLibrary),
        };
        let closure = closure_at(body, &object.site)
            .ok_or_else(|| unreadable("its site is not a closure expression"))?;
        let in_main = object.site.unit == Unit::Main;

        let mut reads = Reads::default();
        let mut bound = closure.params.clone();
        reads.block(&closure.body, &mut bound);
        self.reads.effects.extend(reads.effects);
        self.reads.library.extend(reads.library);

        for free in &reads.free {
            let cell = object
                .captures
                .iter()
                .find(|(captured, _)| captured == free)
                .map(|(_, cell)| *cell);
            let (value, objects) = match cell {
                Some(cell) => match left.closure_objects.get(&cell) {
                    Some(Object::Variable(value)) => (value, left.closure_objects),
                    _ => return Err(unreadable("a captured variable is missing")),
                },
                // A closure made in `main` reads the session's variables
                // by name; one made in a declared function has nothing
                // outside its captures.
                None if in_main => {
                    if let Some(value) = left.carried.get(free) {
                        (value, left.carried_objects)
                    } else if let Some(value) = left.closures.get(free) {
                        (value, left.closure_objects)
                    } else if left.not_carried.contains(free) {
                        return Err(NotSaved::Capture {
                            name: free.clone(),
                            why: CaptureRefusal::Task,
                        });
                    } else {
                        // Unbound when the cell ended: unbound when called.
                        continue;
                    }
                }
                None => continue,
            };
            let refuse = |why| NotSaved::Capture {
                name: free.clone(),
                why,
            };
            // A closure no binding names is frozen with the function that
            // holds it.
            if let Some(helper) = closure_held(value, left.values.tag())
                && !self.named.contains_key(&helper)
            {
                match self.bound.get(free) {
                    Some(Held::Helper(held)) if *held == helper => continue,
                    Some(_) => return Err(refuse(CaptureRefusal::Conflict)),
                    None => {}
                }
                let Some(Object::Closure(inner)) = left.closure_objects.get(&helper) else {
                    return Err(unreadable("a captured closure is missing"));
                };
                let token = match (token_closure(value, left.values.tag()), value) {
                    (Some((_, at)), Value::Tuple(members)) => {
                        let mut constants = Vec::with_capacity(members.len());
                        for (index, member) in members.iter().enumerate() {
                            if index == at {
                                constants.push(Expr::Literal(Literal::Absent));
                                continue;
                            }
                            let datum = freeze(member, objects, self.named, &mut Vec::new())
                                .map_err(refuse)?;
                            let constant = constant(&datum).ok_or_else(|| {
                                refuse(CaptureRefusal::NoConstant {
                                    kind: member.kind(),
                                })
                            })?;
                            constants.push(constant);
                        }
                        Some((constants, at))
                    }
                    _ => None,
                };
                self.bound.insert(free.clone(), Held::Helper(helper));
                let code = self.absorb(inner)?.clone();
                self.helpers.push(Helper {
                    name: free.clone(),
                    code,
                    token,
                });
                continue;
            }
            let datum = freeze(value, objects, self.named, &mut Vec::new()).map_err(refuse)?;
            match self.bound.get(free) {
                Some(Held::Data(held)) if *held == datum => {}
                Some(_) => return Err(refuse(CaptureRefusal::Conflict)),
                None => {
                    self.bound.insert(free.clone(), Held::Data(datum));
                }
            }
        }
        Ok(closure)
    }
}

/// The closure expression at `site` of the unit whose body is `body`.
fn closure_at<'a>(body: &'a [Stmt], site: &Site) -> Option<&'a lash_kernel_doc::Closure> {
    let mut node = Node::Block(body);
    for step in &site.path {
        node = node.children().into_iter().nth(*step as usize)?;
    }
    match node {
        Node::Expr(Expr::Closure(closure)) => Some(closure),
        _ => None,
    }
}

/// A value as a tree of data, with each closure a binding holds as a
/// reference to that binding's function.
fn freeze(
    value: &Value,
    objects: &BTreeMap<ObjectId, Object>,
    named: &BTreeMap<ObjectId, Name>,
    within: &mut Vec<ObjectId>,
) -> Result<Datum, CaptureRefusal> {
    let no_constant = |kind| Err(CaptureRefusal::NoConstant { kind });
    let object = |id: &ObjectId, within: &mut Vec<ObjectId>| {
        if within.contains(id) {
            return Err(CaptureRefusal::Cyclic);
        }
        within.push(*id);
        let each = |values: &[Value], within: &mut Vec<ObjectId>| {
            values
                .iter()
                .map(|value| freeze(value, objects, named, within))
                .collect::<Result<Vec<_>, _>>()
        };
        let datum = match objects.get(id) {
            Some(Object::List(items)) => Datum::List(each(items, within)?),
            Some(Object::Set(items)) => Datum::Set(each(items, within)?),
            Some(Object::Map(entries)) => Datum::Map(
                entries
                    .iter()
                    .map(|(key, value)| {
                        Ok((
                            freeze(key, objects, named, within)?,
                            freeze(value, objects, named, within)?,
                        ))
                    })
                    .collect::<Result<_, CaptureRefusal>>()?,
            ),
            Some(Object::Record(fields)) => Datum::Record(
                fields
                    .iter()
                    .map(|(field, value)| {
                        Ok((field.clone(), freeze(value, objects, named, within)?))
                    })
                    .collect::<Result<_, CaptureRefusal>>()?,
            ),
            Some(Object::Closure(_) | Object::Variable(_)) | None => {
                return Err(CaptureRefusal::Function);
            }
        };
        within.pop();
        Ok(datum)
    };
    match value {
        Value::Null => Ok(Datum::Null),
        Value::Absent => Ok(Datum::Absent),
        Value::Bool(flag) => Ok(Datum::Bool(*flag)),
        Value::Int(integer) => Ok(Datum::Int(integer.clone())),
        Value::Float(float) => Ok(Datum::Float(*float)),
        Value::Text(text) => Ok(Datum::Text(text.to_string())),
        Value::Bytes(bytes) => Ok(Datum::Bytes(bytes.clone())),
        Value::Tuple(members) => Ok(Datum::Tuple(
            members
                .iter()
                .map(|member| freeze(member, objects, named, within))
                .collect::<Result<_, _>>()?,
        )),
        Value::List(id) | Value::Map(id) | Value::Set(id) | Value::Record(id) => object(id, within),
        Value::Closure(id) => match named.get(id) {
            Some(name) => Ok(Datum::Function(name.clone())),
            None => Err(CaptureRefusal::Function),
        },
        Value::Function(name) => Ok(Datum::Function(name.clone())),
        Value::Task(_) | Value::Ref(lash_kernel_doc::Identity::Task(_)) => {
            Err(CaptureRefusal::Task)
        }
        Value::Timestamp(_) | Value::Error(_) | Value::Handle(_) | Value::Ref(_) => {
            no_constant(value.kind())
        }
    }
}

/// The constant a frozen datum is written as.
fn constant(datum: &Datum) -> Option<Expr> {
    let each = |items: &[Datum]| items.iter().map(constant).collect::<Option<Vec<_>>>();
    Some(match datum {
        Datum::Null => Expr::Literal(Literal::Null),
        Datum::Absent => Expr::Literal(Literal::Absent),
        Datum::Bool(flag) => Expr::Literal(Literal::Bool(*flag)),
        Datum::Int(integer) => Expr::Literal(Literal::Int(integer.clone())),
        Datum::Float(float) => Expr::Literal(Literal::Float(*float)),
        Datum::Text(text) => Expr::Literal(Literal::Text(text.clone())),
        Datum::Bytes(bytes) => Expr::Literal(Literal::Bytes(bytes.clone())),
        Datum::Function(name) => Expr::Literal(Literal::Function(name.clone())),
        Datum::Tuple(items) => Expr::Tuple(each(items)?),
        Datum::List(items) => Expr::List(each(items)?),
        Datum::Set(items) => Expr::Set(each(items)?),
        Datum::Map(entries) => Expr::Map(
            entries
                .iter()
                .map(|(key, value)| {
                    Some(MapEntry {
                        key: constant(key)?,
                        value: constant(value)?,
                    })
                })
                .collect::<Option<_>>()?,
        ),
        Datum::Record(fields) => Expr::Record(
            fields
                .iter()
                .map(|(field, value)| {
                    Some(RecordEntry {
                        field: field.clone(),
                        value: constant(value)?,
                    })
                })
                .collect::<Option<_>>()?,
        ),
        Datum::Number(_) | Datum::Timestamp(_) | Datum::Error(_) | Datum::Handle(_) => {
            return None;
        }
    })
}

fn functions_named(datum: &Datum, out: &mut BTreeSet<Name>) {
    match datum {
        Datum::Function(name) => {
            out.insert(name.clone());
        }
        Datum::Tuple(items) | Datum::List(items) | Datum::Set(items) => {
            items.iter().for_each(|item| functions_named(item, out));
        }
        Datum::Map(entries) => entries.iter().for_each(|(key, value)| {
            functions_named(key, out);
            functions_named(value, out);
        }),
        Datum::Record(fields) => fields
            .iter()
            .for_each(|(_, value)| functions_named(value, out)),
        _ => {}
    }
}

/// What a body reads of the world outside it.
#[derive(Default)]
struct Reads {
    /// The variables it names and does not declare.
    free: BTreeSet<Name>,
    effects: BTreeSet<EffectName>,
    library: BTreeSet<FunctionId>,
    /// The declared functions it names.
    declared: BTreeSet<Name>,
}

impl Reads {
    fn name(&mut self, name: &Name, bound: &[Name]) {
        if !bound.contains(name) {
            self.free.insert(name.clone());
        }
    }

    /// Walks a block in a scope of its own: what it declares ends with it.
    fn block(&mut self, block: &[Stmt], bound: &mut Vec<Name>) {
        let outer = bound.len();
        for stmt in block {
            self.stmt(stmt, bound);
        }
        bound.truncate(outer);
    }

    fn scoped(&mut self, binding: &Name, block: &[Stmt], bound: &mut Vec<Name>) {
        bound.push(binding.clone());
        self.block(block, bound);
        bound.pop();
    }

    fn stmt(&mut self, stmt: &Stmt, bound: &mut Vec<Name>) {
        match stmt {
            Stmt::Let { name, value } => {
                // The right-hand side does not see the variable it binds.
                self.rhs(value, bound);
                bound.push(name.clone());
            }
            Stmt::Assign { place, value } => {
                match place {
                    Place::Variable(name) => self.name(name, bound),
                    Place::Member(member) => self.member(member, bound),
                }
                self.rhs(value, bound);
            }
            Stmt::Remove { member } => self.member(member, bound),
            Stmt::Do { action } => self.action(action, bound),
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                self.expr(condition, bound);
                self.block(then_block, bound);
                self.block(else_block, bound);
            }
            Stmt::For {
                binding,
                iterable,
                body,
            } => {
                self.expr(iterable, bound);
                self.scoped(binding, body, bound);
            }
            Stmt::While { condition, body } => {
                self.expr(condition, bound);
                self.block(body, bound);
            }
            Stmt::Break | Stmt::Continue => {}
            Stmt::Return { value }
            | Stmt::Throw { value }
            | Stmt::Print { value }
            | Stmt::Finish { value }
            | Stmt::Fail { value } => self.expr(value, bound),
            Stmt::Try(scope) => {
                self.block(&scope.body, bound);
                if let Some(catch) = &scope.catch {
                    self.scoped(&catch.binding, &catch.body, bound);
                }
                if let Some(finally) = &scope.finally {
                    self.block(finally, bound);
                }
            }
        }
    }

    fn rhs(&mut self, rhs: &Rhs, bound: &mut Vec<Name>) {
        match rhs {
            Rhs::Expr(expr) => self.expr(expr, bound),
            Rhs::Action(action) => self.action(action, bound),
        }
    }

    fn atom(&mut self, atom: &Atom, bound: &[Name]) {
        match atom {
            Atom::Variable(name) => self.name(name, bound),
            Atom::Literal(Literal::Function(name)) => {
                self.declared.insert(name.clone());
            }
            Atom::Literal(_) => {}
        }
    }

    fn action(&mut self, action: &Action, bound: &[Name]) {
        match action {
            Action::Call { callee, args } | Action::Spawn { callee, args } => {
                match callee {
                    Callee::Declared(name) => {
                        self.declared.insert(name.clone());
                    }
                    Callee::Value(name) => self.name(name, bound),
                    Callee::Library(function) => {
                        self.library.insert(*function);
                    }
                }
                args.iter().for_each(|arg| self.atom(arg, bound));
            }
            Action::Perform { effect, args, .. } => {
                self.effects.insert(effect.clone());
                args.iter().for_each(|arg| self.atom(arg, bound));
            }
            Action::Sleep { duration: atom }
            | Action::Join { task: atom }
            | Action::JoinMany { tasks: atom, .. }
            | Action::Cancel { task: atom } => self.atom(atom, bound),
            Action::Yield => {}
        }
    }

    fn member(&mut self, member: &Member, bound: &mut Vec<Name>) {
        match member {
            Member::Field { target, .. } => self.expr(target, bound),
            Member::Index { target, index } => {
                self.expr(target, bound);
                self.expr(index, bound);
            }
        }
    }

    fn expr(&mut self, expr: &Expr, bound: &mut Vec<Name>) {
        match expr {
            Expr::Literal(Literal::Function(name)) => {
                self.declared.insert(name.clone());
            }
            Expr::Literal(_) | Expr::Clock | Expr::Random => {}
            Expr::Variable(name) => self.name(name, bound),
            Expr::Tuple(items) | Expr::List(items) | Expr::Set(items) => {
                items.iter().for_each(|item| self.expr(item, bound));
            }
            Expr::Map(entries) => entries.iter().for_each(|entry| {
                self.expr(&entry.key, bound);
                self.expr(&entry.value, bound);
            }),
            Expr::Record(entries) => entries
                .iter()
                .for_each(|entry| self.expr(&entry.value, bound)),
            Expr::Member(member) => self.member(member, bound),
            Expr::Closure(closure) => {
                let outer = bound.len();
                bound.extend(closure.params.iter().cloned());
                self.block(&closure.body, bound);
                bound.truncate(outer);
            }
            Expr::Call { function, args } => {
                self.library.insert(*function);
                args.iter().for_each(|arg| self.expr(arg, bound));
            }
            Expr::Read(read) => {
                self.expr(&read.handle, bound);
                self.expr(&read.request, bound);
            }
        }
    }
}

/// Why a saved function cannot be used in an environment.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unusable {
    /// It, or one it uses, names a function the session does not hold.
    #[error("the saved function `{function}` uses `{needs}`, which is not a saved function here")]
    Missing { function: Name, needs: Name },
    /// It performs an effect the environment does not offer.
    #[error("the saved function `{function}` calls `{effect}`, which this session does not offer")]
    EffectMissing { function: Name, effect: EffectName },
    /// The environment offers the effect under another signature.
    #[error(
        "the saved function `{function}` calls `{effect}` under a signature this session does not offer it under"
    )]
    EffectSignature { function: Name, effect: EffectName },
    /// The effect's call now ends the turn. A saved function was kept
    /// because none of its effects did (`NotSaved::PerformsControl`), so
    /// what the effect declares has changed since.
    #[error(
        "the saved function `{function}` calls `{effect}`, which now ends the turn; a function cannot end it"
    )]
    EffectControls { function: Name, effect: EffectName },
    /// It calls a library function the environment has not installed.
    #[error(
        "the saved function `{function}` needs the library function `{library}`, which is not installed"
    )]
    LibraryMissing { function: Name, library: String },
    /// It was written under another number policy than the document's.
    #[error("the saved function `{function}` decodes numbers by another policy than this session")]
    Numbers { function: Name },
    /// The document already declares another function under its name.
    #[error("the document already declares a function `{function}`")]
    Declared { function: Name },
    /// A capture has no constant form (a stored value this build did not
    /// write).
    #[error(
        "the saved function `{function}` holds a capture `{capture}` no constant is written for"
    )]
    Capture { function: Name, capture: Name },
    /// It was held in a function value this dialect does not make: a token
    /// in a dialect whose function values are bare, or one of another tag.
    #[error("the saved function `{function}` is held in a value this dialect does not make")]
    Token { function: Name },
}

impl SavedFunction {
    /// The function's parameters and body, free in the captures' names.
    pub fn definition(&self) -> Option<&Function> {
        self.document.functions.get(&self.name)
    }

    /// The saved functions this one names: in a capture, or in its code.
    pub fn needs(&self) -> BTreeSet<Name> {
        let mut needs = BTreeSet::new();
        for datum in self.captures.values() {
            functions_named(datum, &mut needs);
        }
        if let Some(definition) = self.definition() {
            let mut reads = Reads::default();
            let mut bound = definition.params.clone();
            reads.block(&definition.body, &mut bound);
            needs.extend(reads.declared);
        }
        needs
    }

    /// The function as a document declares it: each capture bound to its
    /// constant, then the body.
    ///
    /// # Errors
    ///
    /// [`Unusable::Capture`] for a capture with no constant form.
    pub fn declared(&self) -> Result<Function, Unusable> {
        let Some(definition) = self.definition() else {
            return Err(Unusable::Missing {
                function: self.name.clone(),
                needs: self.name.clone(),
            });
        };
        let mut body = Vec::with_capacity(self.captures.len() + definition.body.len());
        for (capture, datum) in &self.captures {
            let value = constant(datum).ok_or_else(|| Unusable::Capture {
                function: self.name.clone(),
                capture: capture.clone(),
            })?;
            body.push(Stmt::Let {
                name: capture.clone(),
                value: Rhs::Expr(value),
            });
        }
        body.extend(definition.body.iter().cloned());
        Ok(Function {
            params: definition.params.clone(),
            body,
        })
    }

    /// The same function under another name.
    #[must_use]
    pub fn renamed(&self, name: &Name) -> Self {
        let mut renamed = self.clone();
        if let Some(definition) = renamed.document.functions.remove(&self.name) {
            renamed.document.functions.insert(name.clone(), definition);
        }
        renamed.name = name.clone();
        renamed
    }

    /// What a cell that uses the function binds each of its names to: the
    /// token its dialect held it in, around the reference to its
    /// declaration. `None` where that reference is its value.
    pub fn value(&self) -> Option<Expr> {
        let function = Expr::Literal(Literal::Function(self.name.clone()));
        self.token.as_ref().map(|token| token.expr(function))
    }
}

/// The functions `roots` are bindings of, and every saved function they
/// use by name, each once, by the name it is declared under.
///
/// # Errors
///
/// [`Unusable::Missing`] for a name `functions` does not hold.
pub fn closure_of<'a>(
    functions: &'a BTreeMap<Name, SavedFunction>,
    roots: &BTreeSet<Name>,
) -> Result<BTreeMap<Name, &'a SavedFunction>, Unusable> {
    let mut used = BTreeMap::new();
    let mut pending: Vec<(Name, Name)> = roots
        .iter()
        .map(|root| (root.clone(), root.clone()))
        .collect();
    while let Some((by, name)) = pending.pop() {
        if used.contains_key(&name) {
            continue;
        }
        let function = functions.get(&name).ok_or_else(|| Unusable::Missing {
            function: by.clone(),
            needs: name.clone(),
        })?;
        if used.contains_key(&function.name) {
            continue;
        }
        pending.extend(
            function
                .needs()
                .into_iter()
                .map(|needed| (function.name.clone(), needed)),
        );
        used.insert(function.name.clone(), function);
    }
    Ok(used)
}

/// Declares the functions `roots` are bindings of, and every saved
/// function they use, in `document`, and adds what they require to its
/// manifest. `values` are what the dialect's function values are,
/// `effects` the effects the environment offers, `controls` the ones among
/// them whose call ends the turn, and `installed` says whether it holds a
/// library function.
///
/// # Errors
///
/// [`Unusable`], naming the function and what the environment lacks.
pub fn install(
    document: &mut Document,
    functions: &BTreeMap<Name, SavedFunction>,
    roots: &BTreeSet<Name>,
    values: &FunctionValues,
    effects: &BTreeMap<EffectName, Signature>,
    controls: &BTreeMap<EffectName, BTreeSet<crate::EffectControl>>,
    installed: &dyn Fn(&FunctionId) -> bool,
) -> Result<(), Unusable> {
    for (name, function) in closure_of(functions, roots)? {
        let unusable = |unusable: fn(Name) -> Unusable| unusable(name.clone());
        if function
            .token
            .as_ref()
            .map(|token| token.tag.as_str())
            .is_some_and(|tag| values.tag() != Some(tag))
        {
            return Err(unusable(|function| Unusable::Token { function }));
        }
        if function.document.manifest.numbers != document.manifest.numbers {
            return Err(unusable(|function| Unusable::Numbers { function }));
        }
        for (effect, signature) in &function.document.manifest.effects {
            // A tool that now ends the turn also answers nothing, so its
            // signature changed too: the control is the cause named.
            if effects.contains_key(effect) && controls.contains_key(effect) {
                return Err(Unusable::EffectControls {
                    function: name,
                    effect: effect.clone(),
                });
            }
            match effects.get(effect) {
                None => {
                    return Err(Unusable::EffectMissing {
                        function: name,
                        effect: effect.clone(),
                    });
                }
                Some(offered) if offered != signature => {
                    return Err(Unusable::EffectSignature {
                        function: name,
                        effect: effect.clone(),
                    });
                }
                Some(_) => {}
            }
            document
                .manifest
                .effects
                .insert(effect.clone(), signature.clone());
        }
        for (library, listed) in &function.document.manifest.functions {
            if !installed(library) {
                return Err(Unusable::LibraryMissing {
                    function: name,
                    library: listed.to_string(),
                });
            }
            document.manifest.functions.insert(*library, listed.clone());
        }
        if document.functions.contains_key(&name) {
            return Err(unusable(|function| Unusable::Declared { function }));
        }
        document.functions.insert(name, function.declared()?);
    }
    Ok(())
}
