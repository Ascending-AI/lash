//! Closed plain-record definitions, alongside the ordinary (trusted) types.
//! Facts are compiler-only. Mutated property names are deliberately global:
//! a write through an unknown alias must invalidate the same nested proof.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use lash_kernel_doc::Name;

use crate::adapter::{CallArg, Expr, MemberProperty, ObjectProperty, PropertyKey};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PlainRecord(BTreeMap<String, PlainRecord>, usize);

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
        let nodes = 1 + fields.values().map(|value| value.1).sum::<usize>();
        Self(fields, nodes)
    }
}

#[derive(Default)]
pub(super) struct Collector<'a> {
    definitions: HashMap<&'a str, Vec<Option<&'a Expr>>>,
    excluded: BTreeSet<String>,
    mutated: BTreeSet<String>,
    dynamic: bool,
    escapes: Vec<&'a Expr>,
    calls: Vec<(&'a Expr, &'a [CallArg])>,
    awaits: Vec<&'a Expr>,
}

#[derive(Debug, Default)]
pub(super) struct Facts {
    enabled: bool,
    names: HashMap<String, PlainRecord>,
    mutated: BTreeSet<String>,
    dynamic: bool,
}

impl Facts {
    pub(super) fn of(&self, expr: &Expr) -> Option<PlainRecord> {
        if !self.enabled {
            return None;
        }
        match expr {
            Expr::Ident(name, _) => self.names.get(name).cloned(),
            Expr::As { value, .. } => self.of(value),
            Expr::Object(properties) => {
                let mut fields = BTreeMap::new();
                for property in properties {
                    let ObjectProperty::KeyValue(PropertyKey::Static(key), value) = property else {
                        return None;
                    };
                    // Duplicate keys use the last definition, including an
                    // opaque one. Never retain a superseded field proof.
                    fields.remove(key);
                    if !self.dynamic
                        && !self.mutated.contains(key)
                        && let Some(plain) = self.of(value)
                    {
                        fields.insert(key.clone(), plain);
                        if fields.values().map(|value| value.1).sum::<usize>() >= MAX_SHAPE_NODES {
                            return None;
                        }
                    }
                }
                let nodes = 1 + fields.values().map(|value| value.1).sum::<usize>();
                Some(PlainRecord(fields, nodes))
            }
            Expr::Member {
                object, property, ..
            } => self.of(object)?.0.get(property_name(property)?).cloned(),
            _ => None,
        }
    }
}

impl<'a> Collector<'a> {
    pub(super) fn name(&mut self, name: &'a str, value: Option<&'a Expr>) {
        self.definitions.entry(name).or_default().push(value);
    }

    pub(super) fn exclude(&mut self, name: &str) {
        self.excluded.insert(name.to_string());
    }

    pub(super) fn write(&mut self, property: &MemberProperty) {
        match property_name(property) {
            Some(name) => {
                self.mutated.insert(name.to_string());
            }
            None => self.dynamic = true,
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

    pub(super) fn finish(mut self, session: &BTreeSet<Name>, terminal: &BTreeSet<String>) -> Facts {
        let terminal_call = |callee: &Expr| {
            global_path(callee).is_some_and(|(root, path)| {
                terminal.contains(&path)
                    && !self.definitions.contains_key(root)
                    && !session.contains(&Name::new(root))
            })
        };
        if self
            .awaits
            .iter()
            .any(|value| !matches!(value, Expr::Call { callee, .. } if terminal_call(callee)))
        {
            return Facts::default();
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
            self.excluded.insert(name.as_str().to_string());
        }
        for value in &self.escapes {
            escape(
                value,
                &mut self.excluded,
                &mut self.mutated,
                &mut self.dynamic,
            );
        }
        // Close exclusions in both directions across aliases/structural
        // links. Unknown input anywhere in a connected component rejects it.
        let mut links = Vec::new();
        for (name, definitions) in &self.definitions {
            for value in definitions {
                match value {
                    Some(value) => {
                        let mut referenced = BTreeSet::new();
                        references(value, &mut referenced);
                        links.extend(
                            referenced
                                .into_iter()
                                .map(|other| ((*name).to_string(), other)),
                        );
                    }
                    None => {
                        self.excluded.insert((*name).to_string());
                    }
                }
            }
        }
        // Escaping a field can expose any local stored in it. Globally
        // reject those aliases, including ones hidden by an outer literal.
        for definitions in self.definitions.values() {
            for value in definitions.iter().flatten() {
                escaped_fields(value, &self.mutated, self.dynamic, &mut self.excluded);
            }
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
        let mut facts = Facts {
            enabled: true,
            names: HashMap::new(),
            mutated: self.mutated,
            dynamic: self.dynamic,
        };
        // Like the integer-local proof, every definition must agree at the
        // fixed point. Starting empty prevents circular aliases proving
        // themselves; the bound refuses recursively growing shapes.
        for _ in 0..32 {
            let mut next = HashMap::new();
            for (name, definitions) in &self.definitions {
                if self.excluded.contains(*name) {
                    continue;
                }
                let mut shape: Option<PlainRecord> = None;
                let mut proven = true;
                for value in definitions {
                    let Some(part) = value.and_then(|value| facts.of(value)) else {
                        proven = false;
                        break;
                    };
                    shape = Some(shape.map_or_else(|| part.clone(), |shape| shape.meet(&part)));
                }
                if proven && let Some(shape) = shape {
                    next.insert((*name).to_string(), shape);
                }
            }
            if next == facts.names {
                return facts;
            }
            facts.names = next;
        }
        Facts::default()
    }
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
fn references(expr: &Expr, names: &mut BTreeSet<String>) {
    match expr {
        Expr::Ident(name, _) => {
            names.insert(name.clone());
        }
        Expr::Member { object, .. } | Expr::As { value: object, .. } => references(object, names),
        Expr::Object(properties) => {
            for property in properties {
                let (ObjectProperty::KeyValue(_, value) | ObjectProperty::Spread(value)) = property;
                references(value, names);
            }
        }
        Expr::Array(elements) => {
            for element in elements {
                if let crate::adapter::ArrayElement::Value(value)
                | crate::adapter::ArrayElement::Spread(value) = element
                {
                    references(value, names);
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
            references(consequent, names);
            references(alternate, names);
        }
        Expr::Assign { value, .. } => references(value, names),
        _ => {}
    }
}

fn escape(
    expr: &Expr,
    names: &mut BTreeSet<String>,
    fields: &mut BTreeSet<String>,
    dynamic: &mut bool,
) {
    match expr {
        Expr::Member { property, .. } => match property_name(property) {
            Some(name) => {
                fields.insert(name.to_string());
            }
            None => *dynamic = true,
        },
        Expr::As { value, .. } => escape(value, names, fields, dynamic),
        Expr::Object(properties) => {
            for property in properties {
                let (ObjectProperty::KeyValue(_, value) | ObjectProperty::Spread(value)) = property;
                escape(value, names, fields, dynamic);
            }
        }
        Expr::Array(elements) => {
            for element in elements {
                if let crate::adapter::ArrayElement::Value(value)
                | crate::adapter::ArrayElement::Spread(value) = element
                {
                    escape(value, names, fields, dynamic);
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
            escape(consequent, names, fields, dynamic);
            escape(alternate, names, fields, dynamic);
        }
        Expr::Assign { value, .. } => escape(value, names, fields, dynamic),
        _ => references(expr, names),
    }
}

fn escaped_fields(
    expr: &Expr,
    fields: &BTreeSet<String>,
    dynamic: bool,
    names: &mut BTreeSet<String>,
) {
    if let Expr::As { value, .. } = expr {
        escaped_fields(value, fields, dynamic, names);
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
        escaped_fields(consequent, fields, dynamic, names);
        escaped_fields(alternate, fields, dynamic, names);
    } else if let Expr::Object(properties) = expr {
        for property in properties {
            if let ObjectProperty::KeyValue(key, value) = property {
                if dynamic || matches!(key, PropertyKey::Static(key) if fields.contains(key)) {
                    references(value, names);
                }
                escaped_fields(value, fields, dynamic, names);
            }
        }
    }
}
