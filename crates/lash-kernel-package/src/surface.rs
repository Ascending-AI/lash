//! A package's `.surface`: its exports under canonical names, one
//! declaration per line, read into the one `exports` map.
//!
//! ```text
//! function count(q: data_quanta) -> Int = dataframe.count
//! type expr = Record{column: Text}
//!   static column(name: Text) -> expr = dataframe.expr.column
//!   method sort(self: expr, descending: Union(Bool, Absent)) -> expr = dataframe.expr.sort
//!   property name(self: expr) -> Text = dataframe.expr.name
//! error execution_error
//! internal operation read_raw(name: Text) -> Bytes contract "raw/1"
//! operation read_table(name: Text) -> List(Any) contract "table/1" intents read_files
//! ```
//!
//! Slots are kernel types; a bare name of one of the package's own types
//! stands for a value of that type (`brand_type`). `async` before a callable
//! declares a member that may wait; `...name: List(T)` declares the last
//! slot a rest slot. A member's receiver is its first slot. `#` starts a
//! comment line.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::{
    BRAND_FIELD, BUILT_FIELD, Export, Intent, Member, OperationExport, PackagePath, Segment,
    TypeMember, ValueType, Visibility, brand_tuple, brand_type,
};
use lash_kernel_doc::{
    ContractId, FunctionName, OperationRequirement, RecordTypeField, Signature, Type,
    parse_signature, print_type,
};

use crate::{Declared, NameRole, PackageRefusal};

/// Reads `surface` as the exports of the package at `path`.
///
/// # Errors
///
/// [`PackageRefusal::NotCanonical`] for a segment off the grammar,
/// [`PackageRefusal::Repeated`] for a segment declared twice in one scope,
/// and [`PackageRefusal::Source`] for a line that is no declaration.
pub(crate) fn exports(
    path: &PackagePath,
    surface: &str,
) -> Result<BTreeMap<Segment, Export>, PackageRefusal> {
    let reader = Reader {
        path,
        types: declared_types(surface),
    };
    let mut exports: BTreeMap<Segment, (Declared, Export)> = BTreeMap::new();
    // The type whose members the indented lines below it declare.
    let mut open: Option<Segment> = None;
    for (index, raw) in surface.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = index + 1;
        if raw.starts_with(char::is_whitespace) {
            let ty = open
                .as_ref()
                .ok_or_else(|| reader.refuse(at, "a member line stands under no `type`"))?;
            let (segment, declared, member) = reader.type_member(at, ty, line)?;
            let Some((_, Export::Type(value))) = exports.get_mut(ty) else {
                unreachable!("an open type is a declared type");
            };
            if let Some(first) = value.members.get(&segment) {
                return Err(PackageRefusal::Repeated {
                    scope: format!("{path}.{ty}"),
                    segment,
                    first: member_declared(first),
                    second: declared,
                });
            }
            value.members.insert(segment, member);
            continue;
        }
        open = None;
        let (segment, declared, export) = reader.export(at, line)?;
        if let Some((first, _)) = exports.get(&segment) {
            return Err(PackageRefusal::Repeated {
                scope: path.to_string(),
                segment,
                first: *first,
                second: declared,
            });
        }
        if declared == Declared::Type {
            open = Some(segment.clone());
        }
        exports.insert(segment, (declared, export));
    }
    Ok(exports
        .into_iter()
        .map(|(segment, (_, export))| (segment, export))
        .collect())
}

fn member_declared(member: &TypeMember) -> Declared {
    match member {
        TypeMember::Static(_) => Declared::Static,
        TypeMember::Method(_) => Declared::Method,
        TypeMember::Property(_) => Declared::Property,
    }
}

/// The names of the types `surface` declares, as written.
fn declared_types(surface: &str) -> BTreeSet<String> {
    surface
        .lines()
        .filter(|line| !line.starts_with(char::is_whitespace))
        .filter_map(|line| line.trim().strip_prefix("type "))
        .filter_map(|rest| rest.split('=').next())
        .map(|name| name.trim().to_owned())
        .collect()
}

/// Rewrites every bare name of one of `types` that stands as a type in
/// kernel text: a name not followed by `:` or `?`, outside quotes.
pub(crate) fn resolve_types(path: &str, types: &BTreeSet<String>, text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if c == '"' {
            out.push(c);
            let mut escaped = false;
            for (_, c) in chars.by_ref() {
                out.push(c);
                match c {
                    '\\' if !escaped => escaped = true,
                    '"' if !escaped => break,
                    _ => escaped = false,
                }
            }
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let mut end = at + c.len_utf8();
            while let Some((next, c)) = chars.peek().copied() {
                if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
                    end = next + c.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            let word = &text[at..end];
            let named = text[end..].trim_start().starts_with([':', '?']);
            match types.get(word) {
                Some(ty) if !named => out.push_str(&print_type(&brand_type(path, ty))),
                _ => out.push_str(word),
            }
            continue;
        }
        out.push(c);
    }
    out
}

struct Reader<'a> {
    path: &'a PackagePath,
    types: BTreeSet<String>,
}

impl Reader<'_> {
    fn refuse(&self, line: usize, message: &str) -> PackageRefusal {
        PackageRefusal::Source {
            package: self.path.clone(),
            message: format!("surface line {line}: {message}"),
        }
    }

    fn segment(&self, role: NameRole, text: &str) -> Result<Segment, PackageRefusal> {
        Segment::new(text).map_err(|_| PackageRefusal::NotCanonical {
            role,
            text: text.to_owned(),
        })
    }

    fn export(&self, at: usize, line: &str) -> Result<(Segment, Declared, Export), PackageRefusal> {
        let (asynchronous, line) = match line.strip_prefix("async ") {
            Some(rest) => (true, rest.trim_start()),
            None => (false, line),
        };
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim();
        match (word, asynchronous) {
            ("function", _) => {
                let (segment, member) = self.callable(at, NameRole::Export, rest, asynchronous)?;
                Ok((segment, Declared::Function, Export::Function(member)))
            }
            ("type", false) => {
                let (name, representation) = rest
                    .split_once('=')
                    .ok_or_else(|| self.refuse(at, "expected `type <name> = <record type>`"))?;
                let name = name.trim();
                let segment = self.segment(NameRole::Export, name)?;
                let representation = self.representation(at, name, representation.trim())?;
                Ok((
                    segment,
                    Declared::Type,
                    Export::Type(ValueType {
                        representation,
                        members: BTreeMap::new(),
                    }),
                ))
            }
            ("error", false) => Ok((
                self.segment(NameRole::Export, rest)?,
                Declared::Error,
                Export::Error,
            )),
            ("operation", false) => self.operation(at, rest, Visibility::Public),
            ("internal", false) => match rest.strip_prefix("operation ") {
                Some(rest) => self.operation(at, rest.trim(), Visibility::Internal),
                None => Err(self.refuse(at, "expected `internal operation`")),
            },
            _ => Err(self.refuse(
                at,
                "expected `function`, `type`, `error`, `operation` or `internal operation`",
            )),
        }
    }

    fn type_member(
        &self,
        at: usize,
        ty: &Segment,
        line: &str,
    ) -> Result<(Segment, Declared, TypeMember), PackageRefusal> {
        let (asynchronous, line) = match line.strip_prefix("async ") {
            Some(rest) => (true, rest.trim_start()),
            None => (false, line),
        };
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        let (segment, member) = self.callable(at, NameRole::Member, rest.trim(), asynchronous)?;
        let receiver = || {
            member.signature.params.first().map(|param| &param.ty)
                == Some(&brand_type(&self.path.to_string(), ty.as_str()))
        };
        let (declared, member) = match word {
            "static" => (Declared::Static, TypeMember::Static(member)),
            "method" if receiver() => (Declared::Method, TypeMember::Method(member)),
            "property" if receiver() && member.signature.params.len() == 1 && !asynchronous => {
                (Declared::Property, TypeMember::Property(member))
            }
            "method" | "property" => {
                return Err(self.refuse(
                    at,
                    &format!(
                        "a {word}'s first slot is its receiver, of type `{ty}`; a property has no other"
                    ),
                ));
            }
            _ => return Err(self.refuse(at, "expected `static`, `method` or `property`")),
        };
        Ok((segment, declared, member))
    }

    /// `<name>(<slots>) -> <type> = <definition>`.
    fn callable(
        &self,
        at: usize,
        role: NameRole,
        text: &str,
        asynchronous: bool,
    ) -> Result<(Segment, Member), PackageRefusal> {
        let (head, definition) = text.rsplit_once(" = ").ok_or_else(|| {
            self.refuse(at, "expected `<name>(<slots>) -> <type> = <definition>`")
        })?;
        let (name, signature) = head
            .split_once('(')
            .ok_or_else(|| self.refuse(at, "expected the slots in parentheses"))?;
        let segment = self.segment(role, name.trim())?;
        let (signature, rest) = self.signature(at, &format!("({signature}"))?;
        let definition = FunctionName::new(definition.trim())
            .map_err(|_| self.refuse(at, "expected a definition name"))?;
        Ok((
            segment,
            Member {
                definition,
                signature,
                asynchronous,
                rest,
            },
        ))
    }

    /// Reads a signature, `...` marking its last slot a rest slot.
    fn signature(&self, at: usize, text: &str) -> Result<(Signature, bool), PackageRefusal> {
        let rest_name = text.find("...").map(|start| {
            text[start + 3..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect::<String>()
        });
        let text = text.replacen("...", "", 1);
        let resolved = resolve_types(&self.path.to_string(), &self.types, &text);
        let signature = parse_signature(&resolved)
            .map_err(|error| self.refuse(at, &format!("the slots do not read: {error}")))?;
        if let Some(name) = &rest_name {
            let last = signature.params.last();
            if last.is_none_or(|param| {
                param.name.as_str() != name || !matches!(param.ty, Type::List(_))
            }) {
                return Err(self.refuse(at, "a rest slot is the last slot, of a `List` type"));
            }
        }
        Ok((signature, rest_name.is_some()))
    }

    fn representation(&self, at: usize, ty: &str, text: &str) -> Result<Type, PackageRefusal> {
        let resolved = resolve_types(&self.path.to_string(), &self.types, text);
        let signature = parse_signature(&format!("() -> {resolved}"))
            .map_err(|error| self.refuse(at, &format!("the type does not read: {error}")))?;
        let Type::Record(mut record) = signature.result else {
            return Err(self.refuse(at, "a value type is a record type"));
        };
        if record.rest.is_some()
            || record
                .fields
                .iter()
                .any(|field| [BRAND_FIELD, BUILT_FIELD].contains(&field.name.as_str()))
        {
            return Err(self.refuse(
                at,
                "a value type's record names each field it has, and not `__lash_brand` or `__lash_built`",
            ));
        }
        record.fields.push(RecordTypeField {
            name: BRAND_FIELD.to_owned(),
            ty: brand_tuple(&self.path.to_string(), ty),
            optional: false,
        });
        record.fields.push(RecordTypeField {
            name: BUILT_FIELD.to_owned(),
            ty: Type::Bytes,
            optional: false,
        });
        Ok(Type::Record(record))
    }

    /// `<name>(<slots>) -> <type> contract "<id>" [intents <a>, <b>]`.
    fn operation(
        &self,
        at: usize,
        text: &str,
        visibility: Visibility,
    ) -> Result<(Segment, Declared, Export), PackageRefusal> {
        let (head, tail) = text
            .split_once(" contract ")
            .ok_or_else(|| self.refuse(at, "an operation declares its `contract \"<id>\"`"))?;
        let (name, signature) = head
            .split_once('(')
            .ok_or_else(|| self.refuse(at, "expected the slots in parentheses"))?;
        let segment = self.segment(NameRole::Export, name.trim())?;
        let (signature, rest) = self.signature(at, &format!("({signature}"))?;
        if rest {
            return Err(self.refuse(at, "an operation has no rest slot"));
        }
        let tail = tail.trim();
        let contract = tail
            .strip_prefix('"')
            .and_then(|tail| tail.split_once('"'))
            .ok_or_else(|| self.refuse(at, "expected the contract as quoted text"))?;
        let (contract, intents) = contract;
        let intents = match intents.trim() {
            "" => BTreeSet::new(),
            more => more
                .strip_prefix("intents ")
                .ok_or_else(|| self.refuse(at, "expected `intents <name>, …`"))?
                .split(',')
                .map(|intent| Intent(intent.trim().to_owned()))
                .collect(),
        };
        Ok((
            segment,
            Declared::Operation,
            Export::Operation(OperationExport {
                requirement: OperationRequirement {
                    signature,
                    contract: ContractId::new(contract),
                },
                intents,
                visibility,
            }),
        ))
    }
}
