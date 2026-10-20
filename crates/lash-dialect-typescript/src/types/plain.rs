//! Closed plain-record definitions and their primitive fields, alongside the
//! ordinary (trusted) types.
//! Facts are compiler-only and belong to bindings, as the type facts do, so
//! a function's own bindings are proven like the program's. Mutated property
//! names are deliberately global: a write through an unknown alias must
//! invalidate the same nested proof.
//!
//! A binding read or written from a function nested inside the one that
//! declares it is never proven: such a closure can run after its function
//! returned and its record was handed to code this source cannot see.
//!
//! A text is proven by construction to begin with a literal prefix: a
//! string literal, a template literal's first text, a `+` whose left
//! operand is such a text (JavaScript's `+` with a text operand joins), or
//! a binding every value given to it is one of those. A write keyed by
//! such a text keeps the source's proofs when no text with its prefix can
//! be the internal brand or a name some built-in answers
//! (`builtins::ordinary_prefix`); it marks the names with that prefix
//! written.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use lash_kernel_doc::Name;

use super::Ty;
use crate::adapter::{BinaryOp, CallArg, Expr, MemberProperty, ObjectProperty, PropertyKey};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PlainRecord(BTreeMap<String, PlainRecord>, usize, BTreeMap<String, Ty>);

// A small source can describe exponentially many paths through aliases.
// Bound the compiler's expanded proof independently of runtime heap size.
const MAX_SHAPE_NODES: usize = 4096;

impl PlainRecord {
    fn meet(&self, other: &Self) -> Self {
        let fields: BTreeMap<_, _> = self
            .0
            .iter()
            .filter_map(|(key, value)| {
                other
                    .0
                    .get(key)
                    .map(|other| (key.clone(), value.meet(other)))
            })
            .collect();
        let primitives = self
            .2
            .iter()
            .filter(|(key, ty)| other.2.get(*key) == Some(*ty))
            .map(|(key, ty)| (key.clone(), ty.clone()))
            .collect::<BTreeMap<_, _>>();
        let nodes = 1 + primitives.len() + fields.values().map(|value| value.1).sum::<usize>();
        Self(fields, nodes, primitives)
    }
}

/// What a name the source spells stands for: one of its bindings, or a
/// name it does not declare (a session binding or a global).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Key {
    Binding(usize),
    Free(String),
}

/// The binding a spelling of the program names, if the source declares it.
pub(super) type Resolve<'r> = &'r dyn Fn(&str) -> Option<usize>;

fn key(name: &str, resolve: Resolve<'_>) -> Key {
    resolve(name).map_or_else(|| Key::Free(name.to_string()), Key::Binding)
}

/// Property names something may have written or exposed.
#[derive(Clone, Debug, Default)]
struct Names {
    names: BTreeSet<String>,
    /// A number keyed a write or an escaped read. It names only what a
    /// number's text can be, never the internal brand.
    numeric: bool,
    /// A text proven to begin with one of these keyed a write. None is a
    /// prefix of the internal brand.
    prefixes: BTreeSet<String>,
}

impl Names {
    fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
            || (self.numeric && numeric_name(name))
            || self
                .prefixes
                .iter()
                .any(|prefix| name.starts_with(prefix.as_str()))
    }

    fn insert(&mut self, name: &str) {
        self.names.insert(name.to_string());
    }
}

/// Whether `name` may be the text of a number, as a property key spells it:
/// `NaN`, `Infinity`, `-Infinity`, or a digit, after an optional minus
/// sign. This admits more than the canonical spellings, never fewer.
fn numeric_name(name: &str) -> bool {
    let digits = name.strip_prefix('-').unwrap_or(name);
    matches!(name, "NaN" | "Infinity" | "-Infinity")
        || digits.as_bytes().first().is_some_and(u8::is_ascii_digit)
}

#[derive(Default)]
pub(super) struct Collector<'a> {
    /// Each binding's spelling and every value it is given, `None` where
    /// the analysis does not follow it.
    definitions: HashMap<usize, (&'a str, Vec<Option<&'a Expr>>)>,
    /// Bindings that may hold a value no definition gives: an earlier
    /// cell's.
    unfollowed: BTreeSet<usize>,
    /// Every name the source declares or assigns, in any scope.
    defined: BTreeSet<&'a str>,
    excluded: BTreeSet<Key>,
    mutated: Names,
    /// The computed keys of writes; what each may name is settled once the
    /// numeric bindings are known.
    keys: Vec<&'a Expr>,
    dynamic: bool,
    representation_written: bool,
    escapes: Vec<&'a Expr>,
    calls: Vec<(&'a Expr, &'a [CallArg])>,
    awaits: Vec<&'a Expr>,
}

#[derive(Debug, Default)]
pub(super) struct Facts {
    enabled: bool,
    names: HashMap<usize, PlainRecord>,
    primitives: HashMap<usize, Ty>,
    mutated: Names,
    written: Names,
    dynamic: bool,
    definitions: HashMap<String, Expr>,
    /// What each binding's text is proven to begin with. These hold
    /// whether or not the record proofs do: a text cannot change.
    prefixes: HashMap<usize, String>,
}

impl Facts {
    pub(super) fn definition(&self, name: &str) -> Option<&Expr> {
        self.definitions.get(name)
    }

    /// The literal text that `expr`'s value is proven by construction to
    /// begin with: `Some` only where the value is always a text.
    pub(super) fn text_prefix(&self, expr: &Expr, resolve: Resolve<'_>) -> Option<String> {
        text_prefix(expr, &self.prefixes, resolve)
    }

    /// Facts that prove no record, keeping the text proofs.
    fn texts_only(prefixes: HashMap<usize, String>) -> Self {
        Self {
            prefixes,
            ..Self::default()
        }
    }

    pub(super) fn of(&self, expr: &Expr, resolve: Resolve<'_>) -> Option<PlainRecord> {
        if !self.enabled {
            return None;
        }
        match expr {
            Expr::Ident(name, _) => self.names.get(&resolve(name)?).cloned(),
            Expr::As { value, .. } => self.of(value, resolve),
            Expr::Object(properties) => {
                let mut fields = BTreeMap::new();
                let mut primitives = BTreeMap::new();
                for property in properties {
                    let ObjectProperty::KeyValue(PropertyKey::Static(key), value) = property else {
                        return None;
                    };
                    // Opaque/restored kernel data can contain a package
                    // brand, so a field write may seal this record.
                    if key == "__lash_brand" {
                        return None;
                    }
                    // Duplicate keys use the last definition, including an
                    // opaque one. Never retain a superseded field proof.
                    fields.remove(key);
                    primitives.remove(key);
                    // Passing a primitive field's value cannot replace that
                    // field. A write to the field itself still invalidates it;
                    // escaped parents and record spines are rejected below.
                    if !self.dynamic
                        && !self.written.contains(key)
                        && let Some(ty) = self.primitive(value, resolve)
                    {
                        primitives.insert(key.clone(), ty);
                    }
                    if !self.dynamic
                        && !self.mutated.contains(key)
                        && let Some(plain) = self.of(value, resolve)
                    {
                        fields.insert(key.clone(), plain);
                    }
                    if primitives.len() + fields.values().map(|value| value.1).sum::<usize>()
                        >= MAX_SHAPE_NODES
                    {
                        return None;
                    }
                }
                let nodes =
                    1 + primitives.len() + fields.values().map(|value| value.1).sum::<usize>();
                Some(PlainRecord(fields, nodes, primitives))
            }
            Expr::Member {
                object, property, ..
            } => self
                .of(object, resolve)?
                .0
                .get(property_name(property)?)
                .cloned(),
            _ => None,
        }
    }

    pub(super) fn primitive(&self, expr: &Expr, resolve: Resolve<'_>) -> Option<Ty> {
        if !self.enabled {
            return None;
        }
        match expr {
            Expr::String(_) => Some(Ty::Text),
            Expr::Array(_) => Some(Ty::Array),
            Expr::Ident(name, _) => self.primitives.get(&resolve(name)?).cloned(),
            Expr::As { value, .. } => self.primitive(value, resolve),
            Expr::Member {
                object, property, ..
            } => self
                .of(object, resolve)?
                .2
                .get(property_name(property)?)
                .cloned(),
            _ => None,
        }
    }
}

impl<'a> Collector<'a> {
    /// A declaration of, or an assignment to, `binding`, the binding
    /// `name` spells; `None` is a name the source does not declare, whose
    /// value leaves the source's view.
    pub(super) fn name(&mut self, name: &'a str, binding: Option<usize>, value: Option<&'a Expr>) {
        self.defined.insert(name);
        match binding {
            Some(binding) => {
                self.definitions
                    .entry(binding)
                    .or_insert_with(|| (name, Vec::new()))
                    .1
                    .push(value);
            }
            None => {
                self.excluded.insert(Key::Free(name.to_string()));
                self.escapes.extend(value);
            }
        }
    }

    pub(super) fn exclude(&mut self, key: Key) {
        self.excluded.insert(key);
    }

    /// `binding` may hold what an earlier cell left in it.
    pub(super) fn session(&mut self, binding: usize) {
        self.excluded.insert(Key::Binding(binding));
        self.unfollowed.insert(binding);
    }

    /// The prefix every value of each binding is proven to begin with.
    /// Starting from none proven, a binding is proven once each of its
    /// values is under what the round before proved; each round's proofs
    /// hold, and a prefix only grows, so the rounds settle.
    fn text_prefixes(&self, resolve: Resolve<'_>) -> HashMap<usize, String> {
        let mut known = HashMap::new();
        for _ in 0..32 {
            let mut next = HashMap::new();
            for (binding, (_, values)) in &self.definitions {
                if self.unfollowed.contains(binding) {
                    continue;
                }
                let mut common: Option<String> = None;
                for value in values {
                    let Some(prefix) = value.and_then(|value| text_prefix(value, &known, resolve))
                    else {
                        common = None;
                        break;
                    };
                    common = Some(match common {
                        Some(common) => common_prefix(&common, &prefix),
                        None => prefix,
                    });
                }
                if let Some(common) = common {
                    next.insert(*binding, common);
                }
            }
            if next == known {
                break;
            }
            known = next;
        }
        known
    }

    pub(super) fn write(&mut self, property: &'a MemberProperty) {
        match (property_name(property), property) {
            // The internal brand can change representation, not just a
            // nested value, through any alias.
            (Some(name), _) => {
                self.representation_written |= name == "__lash_brand";
                self.mutated.insert(name);
            }
            (None, MemberProperty::Index(key)) => self.keys.push(key),
            (None, MemberProperty::Field(_)) => {}
        }
    }

    pub(super) fn escape(&mut self, expr: &'a Expr) {
        self.escapes.push(expr);
    }

    pub(super) fn call(&mut self, callee: &'a Expr, args: &'a [CallArg]) {
        self.calls.push((callee, args));
    }

    pub(super) fn await_value(&mut self, value: &'a Expr) {
        self.awaits.push(value);
    }

    /// `resolve` gives the binding a spelling names, and `numeric` whether
    /// an expression's value is always a number.
    pub(super) fn finish(
        mut self,
        session: &BTreeSet<Name>,
        terminal: &BTreeSet<String>,
        resolve: Resolve<'_>,
        numeric: &dyn Fn(&Expr) -> bool,
    ) -> Facts {
        let prefixes = self.text_prefixes(resolve);
        // A key that is always a number names only a number's text, and one
        // proven to begin with an ordinary prefix only texts with it. Any
        // other might be the internal brand field: such a write can change
        // representation through any alias, so retain admission throughout
        // this source.
        for key in &self.keys {
            if numeric(key) {
                self.mutated.numeric = true;
            } else if let Some(prefix) = text_prefix(key, &prefixes, resolve)
                && crate::builtins::ordinary_prefix(&prefix)
            {
                self.mutated.prefixes.insert(prefix);
            } else {
                return Facts::texts_only(prefixes);
            }
        }
        if self.representation_written {
            return Facts::texts_only(prefixes);
        }
        let written = self.mutated.clone();
        let terminal_call = |callee: &Expr| {
            global_path(callee).is_some_and(|(root, path)| {
                terminal.contains(&path)
                    && !self.defined.contains(root)
                    && !session.contains(&Name::new(root))
            })
        };
        if self
            .awaits
            .iter()
            .any(|value| !matches!(value, Expr::Call { callee, .. } if terminal_call(callee)))
        {
            return Facts::texts_only(prefixes);
        }
        for (callee, args) in &self.calls {
            if terminal_call(callee) {
                continue;
            }
            // A method may mutate its receiver, even without arguments.
            self.escapes.push(match callee {
                Expr::Member { object, .. } => object,
                _ => callee,
            });
            for arg in *args {
                let (CallArg::Value(value) | CallArg::Spread(value)) = arg;
                self.escapes.push(value);
            }
        }
        for name in session {
            self.excluded.insert(Key::Free(name.as_str().to_string()));
        }
        let mut escaped = Escaped {
            names: &mut self.excluded,
            fields: &mut self.mutated,
            dynamic: &mut self.dynamic,
            resolve,
            numeric,
        };
        for value in &self.escapes {
            escaped.escape(value);
        }
        // Close exclusions in both directions across aliases/structural
        // links. Unknown input anywhere in a connected component rejects it.
        let mut links = Vec::new();
        for (binding, (_, definitions)) in &self.definitions {
            for value in definitions {
                match value {
                    Some(value) => {
                        let mut referenced = BTreeSet::new();
                        references(value, &mut referenced, resolve);
                        links.extend(
                            referenced
                                .into_iter()
                                .map(|other| (Key::Binding(*binding), other)),
                        );
                    }
                    None => {
                        self.excluded.insert(Key::Binding(*binding));
                    }
                }
            }
        }
        // Escaping a field can expose any local stored in it. Globally
        // reject those aliases, including ones hidden by an outer literal.
        let exposed = |value: &Expr, excluded: &mut BTreeSet<Key>| {
            escaped_fields(value, &self.mutated, self.dynamic, excluded, resolve);
        };
        for (_, definitions) in self.definitions.values() {
            for value in definitions.iter().flatten() {
                exposed(value, &mut self.excluded);
            }
        }
        // An escaped member can expose an alias inside a fresh, unnamed
        // literal too: `call(({ inner: local }).inner)` has no wrapper
        // definition for the pass above to inspect.
        for value in &self.escapes {
            exposed(value, &mut self.excluded);
        }
        loop {
            let before = self.excluded.len();
            for (left, right) in &links {
                if self.excluded.contains(left) || self.excluded.contains(right) {
                    self.excluded.insert(left.clone());
                    self.excluded.insert(right.clone());
                }
            }
            if self.excluded.len() == before {
                break;
            }
        }
        // A path is resolved by its root's name: keep a definition only
        // where one binding has that name.
        let mut spelled: HashMap<&str, usize> = HashMap::new();
        for (name, _) in self.definitions.values() {
            *spelled.entry(name).or_default() += 1;
        }
        let mut facts = Facts {
            enabled: true,
            names: HashMap::new(),
            primitives: HashMap::new(),
            prefixes,
            mutated: self.mutated,
            written,
            dynamic: self.dynamic,
            definitions: self
                .definitions
                .iter()
                .filter_map(|(binding, (name, values))| {
                    if let [Some(value)] = values.as_slice()
                        && matches!(value, Expr::Ident(..) | Expr::Member { .. })
                        && !self.excluded.contains(&Key::Binding(*binding))
                        && spelled.get(name) == Some(&1)
                    {
                        Some(((*name).to_string(), (*value).clone()))
                    } else {
                        None
                    }
                })
                .collect(),
        };
        // Like the integer-local proof, every definition must agree at the
        // fixed point. Starting empty prevents circular aliases proving
        // themselves; the bound refuses recursively growing shapes.
        for _ in 0..32 {
            let mut next = HashMap::new();
            let mut primitives = HashMap::new();
            for (binding, (_, definitions)) in &self.definitions {
                if self.excluded.contains(&Key::Binding(*binding)) {
                    continue;
                }
                let mut primitive = None;
                for value in definitions {
                    let part = value.and_then(|value| facts.primitive(value, resolve));
                    if part.is_none()
                        || primitive
                            .as_ref()
                            .is_some_and(|ty| Some(ty) != part.as_ref())
                    {
                        primitive = None;
                        break;
                    }
                    primitive = part;
                }
                if let Some(ty) = primitive {
                    primitives.insert(*binding, ty);
                }
                let mut shape: Option<PlainRecord> = None;
                let mut proven = true;
                for value in definitions {
                    let Some(part) = value.and_then(|value| facts.of(value, resolve)) else {
                        proven = false;
                        break;
                    };
                    shape = Some(shape.map_or_else(|| part.clone(), |shape| shape.meet(&part)));
                }
                if proven && let Some(shape) = shape {
                    next.insert(*binding, shape);
                }
            }
            if next == facts.names && primitives == facts.primitives {
                return facts;
            }
            facts.names = next;
            facts.primitives = primitives;
        }
        Facts::texts_only(facts.prefixes)
    }
}

/// The literal text `expr`'s value begins with, given what each binding's
/// text is `known` to begin with: `Some` only where the value is always a
/// text. A `+` with a text operand joins its operands' texts, whatever the
/// other converts to.
fn text_prefix(
    expr: &Expr,
    known: &HashMap<usize, String>,
    resolve: Resolve<'_>,
) -> Option<String> {
    match expr {
        Expr::String(text) => Some(text.clone()),
        Expr::Template { quasis, .. } => quasis.first().cloned(),
        Expr::Binary {
            left,
            op: BinaryOp::Add,
            right,
            ..
        } => text_prefix(left, known, resolve)
            .or_else(|| text_prefix(right, known, resolve).map(|_| String::new())),
        Expr::Ident(name, _) => known.get(&resolve(name)?).cloned(),
        Expr::As { value, .. } => text_prefix(value, known, resolve),
        Expr::Conditional {
            consequent,
            alternate,
            ..
        } => Some(common_prefix(
            &text_prefix(consequent, known, resolve)?,
            &text_prefix(alternate, known, resolve)?,
        )),
        _ => None,
    }
}

fn common_prefix(left: &str, right: &str) -> String {
    let end = left
        .char_indices()
        .zip(right.chars())
        .find(|((_, a), b)| a != b)
        .map_or_else(|| left.len().min(right.len()), |((index, _), _)| index);
    left[..end].to_string()
}

fn property_name(property: &MemberProperty) -> Option<&str> {
    match property {
        MemberProperty::Field(name) => Some(name),
        MemberProperty::Index(index) => match index.as_ref() {
            Expr::String(name) => Some(name),
            _ => None,
        },
    }
}

fn global_path(expr: &Expr) -> Option<(&str, String)> {
    match expr {
        Expr::Ident(name, _) => Some((name, name.clone())),
        Expr::Member {
            object,
            property: MemberProperty::Field(field),
            ..
        } => {
            let (root, path) = global_path(object)?;
            Some((root, format!("{path}.{field}")))
        }
        _ => None,
    }
}

/// Only value-carrying forms can create aliases. Other expressions are
/// opaque definitions; their calls/captures are handled by the main collector.
fn references(expr: &Expr, names: &mut BTreeSet<Key>, resolve: Resolve<'_>) {
    match expr {
        Expr::Ident(name, _) => {
            names.insert(key(name, resolve));
        }
        Expr::Member { object, .. } | Expr::As { value: object, .. } => {
            references(object, names, resolve);
        }
        Expr::Object(properties) => {
            for property in properties {
                let (ObjectProperty::KeyValue(_, value) | ObjectProperty::Spread(value)) = property;
                references(value, names, resolve);
            }
        }
        Expr::Array(elements) => {
            for element in elements {
                if let crate::adapter::ArrayElement::Value(value)
                | crate::adapter::ArrayElement::Spread(value) = element
                {
                    references(value, names, resolve);
                }
            }
        }
        Expr::Conditional {
            consequent,
            alternate,
            ..
        }
        | Expr::Logical {
            left: consequent,
            right: alternate,
            ..
        } => {
            references(consequent, names, resolve);
            references(alternate, names, resolve);
        }
        Expr::Assign { value, .. } => references(value, names, resolve),
        _ => {}
    }
}

/// What escaping values expose: the bindings they alias, and the fields
/// they hand out.
struct Escaped<'s, 'r> {
    names: &'s mut BTreeSet<Key>,
    fields: &'s mut Names,
    dynamic: &'s mut bool,
    resolve: Resolve<'r>,
    numeric: &'r dyn Fn(&Expr) -> bool,
}

impl Escaped<'_, '_> {
    fn escape(&mut self, expr: &Expr) {
        match expr {
            Expr::Member { property, .. } => match (property_name(property), property) {
                (Some(name), _) => self.fields.insert(name),
                (None, MemberProperty::Index(key)) if (self.numeric)(key) => {
                    self.fields.numeric = true;
                }
                (None, _) => *self.dynamic = true,
            },
            Expr::As { value, .. } => self.escape(value),
            Expr::Object(properties) => {
                for property in properties {
                    let (ObjectProperty::KeyValue(_, value) | ObjectProperty::Spread(value)) =
                        property;
                    self.escape(value);
                }
            }
            Expr::Array(elements) => {
                for element in elements {
                    if let crate::adapter::ArrayElement::Value(value)
                    | crate::adapter::ArrayElement::Spread(value) = element
                    {
                        self.escape(value);
                    }
                }
            }
            Expr::Conditional {
                consequent,
                alternate,
                ..
            }
            | Expr::Logical {
                left: consequent,
                right: alternate,
                ..
            } => {
                self.escape(consequent);
                self.escape(alternate);
            }
            Expr::Assign { value, .. } => self.escape(value),
            _ => references(expr, self.names, self.resolve),
        }
    }
}

fn escaped_fields(
    expr: &Expr,
    fields: &Names,
    dynamic: bool,
    names: &mut BTreeSet<Key>,
    resolve: Resolve<'_>,
) {
    if let Expr::As { value, .. } | Expr::Assign { value, .. } = expr {
        escaped_fields(value, fields, dynamic, names, resolve);
    } else if let Expr::Member { object, .. } = expr {
        escaped_fields(object, fields, dynamic, names, resolve);
    } else if let Expr::Conditional {
        consequent,
        alternate,
        ..
    }
    | Expr::Logical {
        left: consequent,
        right: alternate,
        ..
    } = expr
    {
        escaped_fields(consequent, fields, dynamic, names, resolve);
        escaped_fields(alternate, fields, dynamic, names, resolve);
    } else if let Expr::Object(properties) = expr {
        for property in properties {
            if let ObjectProperty::KeyValue(key, value) = property {
                if dynamic || matches!(key, PropertyKey::Static(key) if fields.contains(key)) {
                    references(value, names, resolve);
                }
                escaped_fields(value, fields, dynamic, names, resolve);
            }
        }
    }
}
