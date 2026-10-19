//! The argument shape check (FIG-6917): the kernel text a package builder
//! generates so that a member is never handed a value another release
//! built.
//!
//! A brand selects members and is not a capability, so a member checks its
//! arguments itself (K-FN-007). For each value type the builder generates
//! one body function, `<path>.<type>.check_shape(x, at)`. It raises
//! `type_error` for a value of another brand, or none, and
//! `package.stale_value` for a value of this brand whose record is not the
//! type's representation, naming the type and the first path that differs,
//! with the value's building artifact and the checker's own. Every member
//! body begins by walking each slot whose declared type mentions a package
//! value type, by that declared type, to those checks.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::{BRAND_FIELD, BUILT_FIELD, brand_type};
use lash_kernel_doc::{RecordType, Signature, Type};

use crate::package::built_function;
use lash_kernel_dialect::PackagePath;

/// The error kind of a value a release of its package built with another
/// representation.
pub const STALE_VALUE: &str = "package.stale_value";

/// The value types of one package, by name, with their representations.
pub(crate) struct Shapes<'a> {
    path: &'a PackagePath,
    types: BTreeMap<String, &'a Type>,
}

/// Kernel text, and the functions it calls by name.
#[derive(Default)]
pub(crate) struct Text {
    pub(crate) lines: Vec<String>,
    pub(crate) calls: BTreeSet<String>,
    fresh: usize,
}

impl Text {
    fn line(&mut self, depth: usize, line: impl AsRef<str>) {
        self.lines
            .push(format!("{}{}", "  ".repeat(depth), line.as_ref()));
    }

    fn fresh(&mut self) -> String {
        self.fresh += 1;
        format!("__shape_{}", self.fresh)
    }
}

/// How a mismatch outside any package value's own record is reported.
#[derive(Clone, Copy)]
enum Fails {
    /// A slot of another kind than declared: `type_error`.
    Type,
    /// A field of a value of this type's brand: `package.stale_value`.
    Stale,
}

impl<'a> Shapes<'a> {
    pub(crate) fn new(path: &'a PackagePath, types: BTreeMap<String, &'a Type>) -> Self {
        Self { path, types }
    }

    fn check_shape(&self, ty: &str) -> String {
        format!("{}.{ty}.check_shape", self.path)
    }

    /// The type `ty` is a value of, when it is one of this package's.
    fn package_type(&self, ty: &Type) -> Option<&str> {
        let path = self.path.to_string();
        self.types
            .keys()
            .find(|name| brand_type(&path, name) == *ty)
            .map(String::as_str)
    }

    /// Whether `ty` is, or holds, a value of one of this package's types.
    pub(crate) fn mentions(&self, ty: &Type) -> bool {
        if self.package_type(ty).is_some() {
            return true;
        }
        match ty {
            Type::List(item) | Type::Set(item) | Type::Task(item) => self.mentions(item),
            Type::Map(map) => self.mentions(&map.key) || self.mentions(&map.value),
            Type::Tuple(items) | Type::Union(items) => items.iter().any(|item| self.mentions(item)),
            Type::Record(record) => record.fields.iter().any(|field| self.mentions(&field.ty)),
            _ => false,
        }
    }

    /// The types each type's representation holds a value of, so their
    /// checks are defined first.
    pub(crate) fn order(&self) -> Result<Vec<String>, String> {
        let mut order = Vec::new();
        let mut visiting = BTreeSet::new();
        for name in self.types.keys() {
            self.place(name, &mut visiting, &mut order)?;
        }
        Ok(order)
    }

    fn place(
        &self,
        name: &str,
        visiting: &mut BTreeSet<String>,
        order: &mut Vec<String>,
    ) -> Result<(), String> {
        if order.iter().any(|placed| placed == name) {
            return Ok(());
        }
        if !visiting.insert(name.to_owned()) {
            return Err(format!(
                "the value type `{name}` holds itself; a value type's representation holds no value of its own type, directly or through another"
            ));
        }
        let mut held = BTreeSet::new();
        self.held(self.types[name], &mut held);
        for other in held {
            self.place(&other, visiting, order)?;
        }
        visiting.remove(name);
        order.push(name.to_owned());
        Ok(())
    }

    /// The package types `ty`'s representation fields hold.
    fn held(&self, ty: &Type, held: &mut BTreeSet<String>) {
        let Type::Record(record) = ty else { return };
        for field in &record.fields {
            self.mentioned(&field.ty, held);
        }
    }

    fn mentioned(&self, ty: &Type, held: &mut BTreeSet<String>) {
        if let Some(name) = self.package_type(ty) {
            held.insert(name.to_owned());
            return;
        }
        match ty {
            Type::List(item) | Type::Set(item) | Type::Task(item) => self.mentioned(item, held),
            Type::Map(map) => {
                self.mentioned(&map.key, held);
                self.mentioned(&map.value, held);
            }
            Type::Tuple(items) | Type::Union(items) => {
                for item in items {
                    self.mentioned(item, held);
                }
            }
            Type::Record(record) => {
                for field in &record.fields {
                    self.mentioned(&field.ty, held);
                }
            }
            _ => {}
        }
    }

    /// The definition text of `<path>.<ty>.check_shape`, before its `use`
    /// lines.
    pub(crate) fn definition(&self, ty: &str) -> Text {
        let path = self.path.to_string();
        let Type::Record(representation) = self.types[ty] else {
            unreachable!("a value type's representation is a record")
        };
        let mut text = Text::default();
        text.line(
            0,
            format!(
                "function {}(x: Any, at: Text) -> Null",
                self.check_shape(ty)
            ),
        );
        text.line(0, "kernel 1");
        text.line(0, format!("errors \"type_error\", \"{STALE_VALUE}\""));
        text.line(0, "charge sum(16, deep(x))");
        text.line(0, "body {");
        text.calls.insert("brand.of".into());
        text.line(1, "let brand = invoke brand.of(x)");
        let not_this = format!(
            "throw error.new(\"type_error\", text.concat(at, \" is not a {path}.{ty}\"), null)"
        );
        text.line(1, "if same(kind(brand), \"absent\") {");
        text.line(2, &not_this);
        text.line(1, "}");
        text.line(1, format!("if bool.not(same(brand[0], \"{path}\")) {{"));
        text.line(2, &not_this);
        text.line(1, "}");
        text.line(1, format!("if bool.not(same(brand[1], \"{ty}\")) {{"));
        text.line(2, &not_this);
        text.line(1, "}");
        text.line(1, "let built = absent");
        text.line(1, format!("if record.contains(x, \"{BUILT_FIELD}\") {{"));
        text.line(2, format!("set built = x.{BUILT_FIELD}"));
        text.line(1, "}");
        text.calls.insert(built_function(self.path).to_string());
        let fail = Fail {
            ty: format!("{path}.{ty}"),
            built: built_function(self.path).to_string(),
        };
        self.record(&mut text, 1, "x", "at", representation, Fails::Stale, &fail);
        text.line(1, "return null");
        text.line(0, "}");
        text
    }

    /// The checks a member's body begins with: one walk of each slot whose
    /// declared type mentions a package value type.
    pub(crate) fn slots(&self, signature: &Signature) -> Option<(Text, Vec<String>)> {
        let mut text = Text::default();
        let mut checked = Vec::new();
        for param in &signature.params {
            if !self.mentions(&param.ty) {
                continue;
            }
            let at = text.fresh();
            text.line(0, format!("let {at} = \"{}\"", param.name));
            let fail = Fail {
                ty: String::new(),
                built: String::new(),
            };
            self.value(
                &mut text,
                0,
                param.name.as_str(),
                &at,
                &param.ty,
                Fails::Type,
                &fail,
            );
            checked.push(param.name.as_str().to_owned());
        }
        (!checked.is_empty()).then_some((text, checked))
    }

    /// Checks the value `var`, read at the path `at`, against `ty`.
    #[expect(
        clippy::too_many_arguments,
        reason = "one walk threads its place, its target and its failure"
    )]
    fn value(
        &self,
        text: &mut Text,
        depth: usize,
        var: &str,
        at: &str,
        ty: &Type,
        fails: Fails,
        fail: &Fail,
    ) {
        if let Some(name) = self.package_type(ty) {
            text.calls.insert(self.check_shape(name));
            text.line(
                depth,
                format!("do invoke {}({var}, {at})", self.check_shape(name)),
            );
            return;
        }
        if let Type::Union(members) = ty {
            self.union(text, depth, var, at, members, fails, fail);
            return;
        }
        let Some(kinds) = kinds(ty) else { return };
        let held = kind_check(text, depth, var, &kinds);
        text.line(depth, format!("if bool.not({held}) {{"));
        fail.raise(text, depth + 1, fails, at);
        text.line(depth, "}");
        match ty {
            Type::Enum(values) => {
                let held = text.fresh();
                text.line(depth, format!("let {held} = false"));
                for value in values {
                    text.line(depth, format!("if same({var}, {}) {{", quoted(value)));
                    text.line(depth + 1, format!("set {held} = true"));
                    text.line(depth, "}");
                }
                text.line(depth, format!("if bool.not({held}) {{"));
                fail.raise(text, depth + 1, fails, at);
                text.line(depth, "}");
            }
            Type::List(item) | Type::Set(item) if needs_walk(item) => {
                let values = if matches!(ty, Type::Set(_)) {
                    format!("set.values({var})")
                } else {
                    var.to_owned()
                };
                self.each(text, depth, &values, at, item, fails, fail);
            }
            Type::Map(map) if needs_walk(&map.value) => {
                self.each(
                    text,
                    depth,
                    &format!("map.values({var})"),
                    at,
                    &map.value,
                    fails,
                    fail,
                );
            }
            Type::Tuple(items) => {
                text.line(
                    depth,
                    format!("if bool.not(eq(tuple.len({var}), {})) {{", items.len()),
                );
                fail.raise(text, depth + 1, fails, at);
                text.line(depth, "}");
                for (index, item) in items.iter().enumerate() {
                    if !needs_walk(item) {
                        continue;
                    }
                    let (element, place) = (text.fresh(), text.fresh());
                    text.line(depth, format!("let {element} = {var}[{index}]"));
                    text.line(
                        depth,
                        format!("let {place} = text.concat({at}, \"[{index}]\")"),
                    );
                    self.value(text, depth, &element, &place, item, fails, fail);
                }
            }
            Type::Record(record) => self.record(text, depth, var, at, record, fails, fail),
            _ => {}
        }
    }

    /// Checks every element `values` yields against `item`.
    #[expect(
        clippy::too_many_arguments,
        reason = "one walk threads its place, its target and its failure"
    )]
    fn each(
        &self,
        text: &mut Text,
        depth: usize,
        values: &str,
        at: &str,
        item: &Type,
        fails: Fails,
        fail: &Fail,
    ) {
        let (element, place) = (text.fresh(), text.fresh());
        text.line(depth, format!("for {element} in {values} {{"));
        text.line(
            depth + 1,
            format!("let {place} = text.concat({at}, \"[]\")"),
        );
        self.value(text, depth + 1, &element, &place, item, fails, fail);
        text.line(depth, "}");
    }

    /// Checks the record `var` holds exactly `record`'s fields, each of its
    /// declared type.
    #[expect(
        clippy::too_many_arguments,
        reason = "one walk threads its place, its target and its failure"
    )]
    fn record(
        &self,
        text: &mut Text,
        depth: usize,
        var: &str,
        at: &str,
        record: &RecordType,
        fails: Fails,
        fail: &Fail,
    ) {
        let held = text.fresh();
        text.line(depth, format!("let {held} = 0"));
        for field in &record.fields {
            let name = quoted(&field.name);
            let (value, place) = (text.fresh(), text.fresh());
            text.line(
                depth,
                format!("let {place} = text.concat({at}, \".{}\")", field.name),
            );
            text.line(depth, format!("if record.contains({var}, {name}) {{"));
            text.line(depth + 1, format!("set {held} = add({held}, 1)"));
            if field.name != BRAND_FIELD && needs_walk(&field.ty) {
                text.line(
                    depth + 1,
                    format!("let {value} = record.get({var}, {name})"),
                );
                self.value(text, depth + 1, &value, &place, &field.ty, fails, fail);
            }
            if !field.optional {
                text.line(depth, "} else {");
                fail.raise(text, depth + 1, fails, &place);
            }
            text.line(depth, "}");
        }
        if record.rest.is_none() {
            text.line(
                depth,
                format!("if bool.not(eq(record.len({var}), {held})) {{"),
            );
            fail.raise(text, depth + 1, fails, at);
            text.line(depth, "}");
        }
    }

    /// Checks `var` against the one member of `members` its kind selects;
    /// package values among them are told apart by their brands.
    #[expect(
        clippy::too_many_arguments,
        reason = "one walk threads its place, its target and its failure"
    )]
    fn union(
        &self,
        text: &mut Text,
        depth: usize,
        var: &str,
        at: &str,
        members: &[Type],
        fails: Fails,
        fail: &Fail,
    ) {
        if members.iter().any(|member| kinds(member).is_none()) {
            return;
        }
        let matched = text.fresh();
        text.line(depth, format!("let {matched} = false"));
        let path = self.path.to_string();
        for member in members {
            text.line(depth, format!("if bool.not({matched}) {{"));
            let test = match self.package_type(member) {
                Some(name) => {
                    text.calls.insert("brand.of".into());
                    let (brand, test) = (text.fresh(), text.fresh());
                    text.line(depth + 1, format!("let {brand} = invoke brand.of({var})"));
                    text.line(depth + 1, format!("let {test} = false"));
                    text.line(
                        depth + 1,
                        format!("if bool.not(same(kind({brand}), \"absent\")) {{"),
                    );
                    text.line(
                        depth + 2,
                        format!(
                            "if same({brand}[0], \"{path}\") {{ if same({brand}[1], \"{name}\") {{ set {test} = true }} }}"
                        ),
                    );
                    text.line(depth + 1, "}");
                    test
                }
                None => kind_check(text, depth + 1, var, &kinds(member).unwrap_or_default()),
            };
            text.line(depth + 1, format!("if {test} {{"));
            text.line(depth + 2, format!("set {matched} = true"));
            self.value(text, depth + 2, var, at, member, fails, fail);
            text.line(depth + 1, "}");
            text.line(depth, "}");
        }
        text.line(depth, format!("if bool.not({matched}) {{"));
        fail.raise(text, depth + 1, fails, at);
        text.line(depth, "}");
    }
}

/// What a failed check raises.
struct Fail {
    /// The value type whose record is checked, for a stale value.
    ty: String,
    /// The checker's building-artifact function.
    built: String,
}

impl Fail {
    fn raise(&self, text: &mut Text, depth: usize, fails: Fails, at: &str) {
        match fails {
            Fails::Type => text.line(
                depth,
                format!("throw error.new(\"type_error\", text.concat({at}, \" has another type than declared\"), null)"),
            ),
            Fails::Stale => {
                let package = self.ty.rsplit_once('.').map_or("", |(package, _)| package);
                let current = text.fresh();
                text.line(depth, format!("let {current} = invoke {}()", self.built));
                text.line(
                    depth,
                    format!(
                        "throw error.new(\"{STALE_VALUE}\", text.concat(\"{} built by another release of {package} differs at \", text.concat({at}, \"; rebuild it\")), {{type: \"{}\", at: {at}, built: built, current: {current}}})",
                        self.ty, self.ty
                    ),
                );
            }
        }
    }
}

/// The kinds `kind` answers for a value of `ty`, or none when the kernel
/// decoder owns validation for the type.
fn kinds(ty: &Type) -> Option<Vec<&'static str>> {
    Some(match ty {
        Type::Any | Type::Json => return None,
        Type::Null => vec!["null"],
        Type::Absent => vec!["absent"],
        Type::Bool => vec!["bool"],
        Type::Int => vec!["integer"],
        Type::Float => vec!["float"],
        Type::Number => vec!["integer", "float"],
        Type::Text | Type::Enum(_) => vec!["text"],
        Type::Bytes => vec!["bytes"],
        Type::Timestamp => vec!["timestamp"],
        Type::Error => vec!["error"],
        Type::Tuple(_) => vec!["tuple"],
        Type::List(_) => vec!["list"],
        Type::Map(_) => vec!["map"],
        Type::Set(_) => vec!["set"],
        Type::Record(_) => vec!["record"],
        Type::Task(_) => vec!["task"],
        Type::Handle(_) => vec!["handle"],
        Type::Function(_) => vec!["closure", "function"],
        Type::Union(members) => {
            let mut all = Vec::new();
            for member in members {
                all.extend(kinds(member)?);
            }
            all
        }
    })
}

/// Whether a value of `ty` needs more than a kind test.
fn needs_walk(ty: &Type) -> bool {
    !matches!(ty, Type::Any | Type::Json)
}

/// Emits the test of whether `var`'s kind is one of `kinds`; gives the
/// variable holding its answer.
fn kind_check(text: &mut Text, depth: usize, var: &str, kinds: &[&str]) -> String {
    let (kind, held) = (text.fresh(), text.fresh());
    text.line(depth, format!("let {kind} = kind({var})"));
    text.line(depth, format!("let {held} = false"));
    for name in kinds {
        text.line(depth, format!("if same({kind}, \"{name}\") {{"));
        text.line(depth + 1, format!("set {held} = true"));
        text.line(depth, "}");
    }
    held
}

fn quoted(text: &str) -> String {
    format!("{text:?}")
}
