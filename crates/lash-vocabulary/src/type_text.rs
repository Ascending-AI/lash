//! Canonical neutral type and requirement text.
use crate::names::is_identifier;
use crate::{Name, OperationRequirement, Signature, Type};
struct Printer {
    out: String,
}
impl Printer {
    fn name(&mut self, name: &Name) {
        let text = name.as_str();
        if is_identifier(text) && !is_keyword(text) {
            self.out.push_str(text);
        } else {
            write_quoted(text, '`', &mut self.out);
        }
    }
    fn field_name(&mut self, field: &str) {
        if is_identifier(field) {
            self.out.push_str(field);
        } else {
            write_quoted(field, '"', &mut self.out);
        }
    }
    fn requirement(&mut self, requirement: &OperationRequirement) {
        self.signature(&requirement.signature);
        self.out.push_str(" contract ");
        write_quoted(requirement.contract.as_str(), '"', &mut self.out);
    }
    fn signature(&mut self, signature: &Signature) {
        self.out.push('(');
        for (index, param) in signature.params.iter().enumerate() {
            if index > 0 {
                self.out.push_str(", ");
            }
            self.name(&param.name);
            if param.optional {
                self.out.push('?');
            }
            self.out.push_str(": ");
            self.ty(&param.ty);
        }
        self.out.push_str(") -> ");
        self.ty(&signature.result);
    }
    fn types(&mut self, name: &str, types: &[Type]) {
        self.out.push_str(name);
        self.out.push('(');
        for (index, ty) in types.iter().enumerate() {
            if index > 0 {
                self.out.push_str(", ");
            }
            self.ty(ty);
        }
        self.out.push(')');
    }
    fn ty(&mut self, ty: &Type) {
        match ty {
            Type::Any => self.out.push_str("Any"),
            Type::Json => self.out.push_str("Json"),
            Type::Null => self.out.push_str("Null"),
            Type::Absent => self.out.push_str("Absent"),
            Type::Bool => self.out.push_str("Bool"),
            Type::Int => self.out.push_str("Int"),
            Type::Float => self.out.push_str("Float"),
            Type::Number => self.out.push_str("Number"),
            Type::Text => self.out.push_str("Text"),
            Type::Bytes => self.out.push_str("Bytes"),
            Type::Timestamp => self.out.push_str("Timestamp"),
            Type::Error => self.out.push_str("Error"),
            Type::Tuple(members) => self.types("Tuple", members),
            Type::Union(members) => self.types("Union", members),
            Type::List(item) => self.types("List", std::slice::from_ref(item)),
            Type::Set(item) => self.types("Set", std::slice::from_ref(item)),
            Type::Task(item) => self.types("Task", std::slice::from_ref(item)),
            Type::Map(map) => {
                self.out.push_str("Map(");
                self.ty(&map.key);
                self.out.push_str(", ");
                self.ty(&map.value);
                self.out.push(')');
            }
            Type::Handle(kind) => {
                self.out.push_str("Handle(");
                write_quoted(kind, '"', &mut self.out);
                self.out.push(')');
            }
            Type::Enum(members) => {
                self.out.push_str("Enum(");
                for (index, member) in members.iter().enumerate() {
                    if index > 0 {
                        self.out.push_str(", ");
                    }
                    write_quoted(member, '"', &mut self.out);
                }
                self.out.push(')');
            }
            Type::Function(signature) => {
                self.out.push_str("Fn");
                self.signature(signature);
            }
            Type::Record(record) => {
                self.out.push_str("Record{");
                let mut first = true;
                for field in &record.fields {
                    if !std::mem::take(&mut first) {
                        self.out.push_str(", ");
                    }
                    self.field_name(&field.name);
                    if field.optional {
                        self.out.push('?');
                    }
                    self.out.push_str(": ");
                    self.ty(&field.ty);
                }
                if let Some(rest) = &record.rest {
                    if !first {
                        self.out.push_str(", ");
                    }
                    self.out.push_str("..");
                    self.ty(rest);
                }
                self.out.push('}');
            }
        }
    }
}
#[doc(hidden)]
pub fn write_quoted(text: &str, quote: char, out: &mut String) {
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push(quote);
}

#[doc(hidden)]
pub const KEYWORDS: &[&str] = &[
    "absent",
    "all",
    "any",
    "apply",
    "as",
    "await",
    "body",
    "break",
    "by_spelling",
    "call",
    "cancel",
    "catch",
    "charge",
    "clock",
    "continue",
    "do",
    "effect",
    "else",
    "entry",
    "errors",
    "fail",
    "false",
    "finally",
    "finish",
    "float",
    "fn",
    "for",
    "function",
    "guard",
    "metered",
    "if",
    "in",
    "inf",
    "invoke",
    "join",
    "kernel",
    "let",
    "main",
    "map",
    "nan",
    "native",
    "null",
    "numbers",
    "perform",
    "print",
    "private",
    "race",
    "random",
    "read",
    "remove",
    "result",
    "return",
    "set",
    "settled",
    "sleep",
    "spawn",
    "throw",
    "true",
    "try",
    "use",
    "while",
    "yield",
];

#[doc(hidden)]
pub fn is_keyword(word: &str) -> bool {
    KEYWORDS.binary_search(&word).is_ok()
}

pub fn print_type(ty: &Type) -> String {
    let mut p = Printer { out: String::new() };
    p.ty(ty);
    p.out
}
pub fn requirement_text(requirement: &OperationRequirement) -> String {
    let mut p = Printer { out: String::new() };
    p.requirement(requirement);
    p.out
}
