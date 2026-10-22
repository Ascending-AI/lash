//! The authored TypeScript a lifted region prints as: a small tree of the
//! forms the workflow authoring guide writes, and its layout. Operators are
//! parenthesized by precedence alone; captions are the JSDoc blocks the
//! front end reads back ([`crate::captions::spell`]).

use lash_kernel_doc::Caption;

/// An expression, as a model would write it.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Expr {
    /// A number literal, already spelled.
    Number(String),
    Text(String),
    Bool(bool),
    Null,
    Undefined,
    Ident(String),
    /// `target.field`, or `target["field"]` for a field no identifier spells.
    Field(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>),
    New(Box<Expr>, Vec<Expr>),
    /// A prefix operator written with its trailing space where it needs one
    /// (`typeof `, `await `).
    Unary(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Conditional(Box<Expr>, Box<Expr>, Box<Expr>),
    Array(Vec<Expr>),
    Object(Vec<(String, Expr)>),
    Template(Vec<Part>),
    Arrow(Box<Function>),
}

/// A piece of a template literal.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Part {
    Text(String),
    Expr(Expr),
}

/// An arrow function or a nested function declaration's shape.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Function {
    pub(super) is_async: bool,
    pub(super) params: Vec<Param>,
    pub(super) result: Option<Type>,
    pub(super) body: Body,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Body {
    /// An arrow's expression body.
    Expr(Expr),
    Block(Vec<Stmt>),
}

/// A parameter, its annotation and its caption.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Param {
    pub(super) caption: Option<Caption>,
    pub(super) name: String,
    pub(super) optional: bool,
    pub(super) ty: Option<Type>,
}

/// A type annotation an entry's signature is written with.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Type {
    /// A keyword or literal type: `string`, `null`, `undefined`, …
    Word(&'static str),
    Literal(String),
    Array(Box<Type>),
    Union(Vec<Type>),
    Object(Vec<Property>, Option<Box<Type>>),
    /// `Promise<T>`, an async function's result.
    Promise(Box<Type>),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Property {
    pub(super) caption: Option<Caption>,
    pub(super) name: String,
    pub(super) optional: bool,
    pub(super) ty: Type,
}

/// A statement and the caption before it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Stmt {
    pub(super) caption: Option<Caption>,
    pub(super) kind: StmtKind,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum StmtKind {
    Const(String, Expr),
    Let(String, Option<Expr>),
    /// `place op value`: `=`, or a compound operator.
    Assign(Expr, &'static str, Expr),
    Expr(Expr),
    If {
        condition: Expr,
        then: Vec<Stmt>,
        otherwise: Option<Arm>,
    },
    ForOf {
        binding: String,
        iterable: Expr,
        body: Vec<Stmt>,
    },
    While {
        condition: Expr,
        body: Vec<Stmt>,
    },
    Return(Option<Expr>),
    Throw(Expr),
    Break,
    Continue,
    Try {
        body: Vec<Stmt>,
        catch: Option<(Arm, Option<String>)>,
        finally: Option<Arm>,
    },
    Function {
        name: String,
        function: Function,
    },
}

/// A clause arm: `else`, `catch` or `finally`, with its caption.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Arm {
    pub(super) caption: Option<Caption>,
    pub(super) body: Vec<Stmt>,
}

/// A top-level function of the module.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Item {
    pub(super) is_async: bool,
    pub(super) caption: Option<Caption>,
    pub(super) name: String,
    pub(super) params: Vec<Param>,
    pub(super) result: Option<Type>,
    pub(super) body: Vec<Stmt>,
}

/// Whether `name` can be written as a binding or a reference.
pub(super) fn identifier(name: &str) -> bool {
    crate::printer::property_identifier(name)
        && !crate::adapter::is_strict_reserved_word(name)
        && !matches!(name, "await" | "arguments" | "eval" | "undefined" | "k")
}

/// A number as TypeScript reads it back to the same float: integral
/// values without a fraction.
pub(super) fn number(value: f64) -> Option<String> {
    if !value.is_finite() {
        return None;
    }
    let text = if value == value.trunc() && value.abs() < 1e21 {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "an integral float below 1e21 fits an i128 exactly"
        )]
        let integral = value as i128;
        if integral == 0 && value.is_sign_negative() {
            return None;
        }
        integral.to_string()
    } else {
        let text = value.to_string();
        if text.parse::<f64>().ok() != Some(value) {
            return None;
        }
        text
    };
    Some(text)
}

const INDENT: &str = "  ";

/// Writes items and statements with two-space indentation.
#[derive(Default)]
pub(super) struct Writer {
    pub(super) out: String,
    indent: usize,
}

impl Writer {
    fn pad(&mut self) {
        for _ in 0..self.indent {
            self.out.push_str(INDENT);
        }
    }

    fn caption(&mut self, caption: Option<&Caption>) {
        if let Some(caption) = caption {
            let indent = INDENT.repeat(self.indent);
            self.out.push_str(&crate::captions::spell(caption, &indent));
        }
    }

    pub(super) fn item(&mut self, item: &Item) {
        self.pad();
        self.caption(item.caption.as_ref());
        self.out.push_str(if item.is_async {
            "export async function "
        } else {
            "export function "
        });
        self.out.push_str(&item.name);
        self.params(&item.params);
        if let Some(result) = &item.result {
            self.out.push_str(": ");
            self.ty(&Type::Promise(Box::new(result.clone())));
        }
        self.out.push(' ');
        self.block(&item.body);
        self.out.push('\n');
    }

    /// The statements of a module's or a cell's top level.
    pub(super) fn top_level(&mut self, statements: &[Stmt]) {
        for statement in statements {
            self.statement(statement);
        }
    }

    fn params(&mut self, params: &[Param]) {
        let multiline = params.iter().any(|param| {
            param.caption.is_some() || param.ty.as_ref().is_some_and(Type::has_caption)
        });
        if !multiline {
            self.out.push('(');
            for (index, param) in params.iter().enumerate() {
                if index > 0 {
                    self.out.push_str(", ");
                }
                self.param(param);
            }
            self.out.push(')');
            return;
        }
        self.out.push_str("(\n");
        self.indent += 1;
        for param in params {
            self.pad();
            self.caption(param.caption.as_ref());
            self.param(param);
            self.out.push_str(",\n");
        }
        self.indent -= 1;
        self.pad();
        self.out.push(')');
    }

    fn param(&mut self, param: &Param) {
        self.out.push_str(&param.name);
        if let Some(ty) = &param.ty {
            self.out.push_str(if param.optional { "?: " } else { ": " });
            self.ty(ty);
        }
    }

    fn ty(&mut self, ty: &Type) {
        match ty {
            Type::Word(word) => self.out.push_str(word),
            Type::Literal(value) => self.out.push_str(&text_literal(value)),
            Type::Array(item) => {
                let grouped = matches!(**item, Type::Union(_));
                if grouped {
                    self.out.push('(');
                }
                self.ty(item);
                if grouped {
                    self.out.push(')');
                }
                self.out.push_str("[]");
            }
            Type::Union(members) => {
                for (index, member) in members.iter().enumerate() {
                    if index > 0 {
                        self.out.push_str(" | ");
                    }
                    self.ty(member);
                }
            }
            Type::Promise(item) => {
                self.out.push_str("Promise<");
                self.ty(item);
                self.out.push('>');
            }
            Type::Object(properties, rest) if !ty.has_caption() => {
                if properties.is_empty() && rest.is_none() {
                    self.out.push_str("{}");
                    return;
                }
                self.out.push_str("{ ");
                for (index, property) in properties.iter().enumerate() {
                    if index > 0 {
                        self.out.push(' ');
                    }
                    self.property(property);
                }
                if let Some(rest) = rest {
                    self.out.push_str(" [key: string]: ");
                    self.ty(rest);
                    self.out.push(';');
                }
                self.out.push_str(" }");
            }
            Type::Object(properties, rest) => {
                self.out.push_str("{\n");
                self.indent += 1;
                for property in properties {
                    self.pad();
                    self.caption(property.caption.as_ref());
                    self.property(property);
                    self.out.push('\n');
                }
                if let Some(rest) = rest {
                    self.pad();
                    self.out.push_str("[key: string]: ");
                    self.ty(rest);
                    self.out.push_str(";\n");
                }
                self.indent -= 1;
                self.pad();
                self.out.push('}');
            }
        }
    }

    fn property(&mut self, property: &Property) {
        self.out.push_str(&property_key(&property.name));
        self.out
            .push_str(if property.optional { "?: " } else { ": " });
        self.ty(&property.ty);
        self.out.push(';');
    }

    fn block(&mut self, statements: &[Stmt]) {
        if statements.is_empty() {
            self.out.push_str("{}");
            return;
        }
        self.out.push_str("{\n");
        self.indent += 1;
        for statement in statements {
            self.statement(statement);
        }
        self.indent -= 1;
        self.pad();
        self.out.push('}');
    }

    fn statement(&mut self, statement: &Stmt) {
        self.pad();
        self.caption(statement.caption.as_ref());
        self.statement_kind(&statement.kind);
        self.out.push('\n');
    }

    fn statement_kind(&mut self, kind: &StmtKind) {
        match kind {
            StmtKind::Const(name, value) => {
                self.out.push_str("const ");
                self.out.push_str(name);
                self.out.push_str(" = ");
                self.expr(value, ASSIGN);
                self.out.push(';');
            }
            StmtKind::Let(name, value) => {
                self.out.push_str("let ");
                self.out.push_str(name);
                if let Some(value) = value {
                    self.out.push_str(" = ");
                    self.expr(value, ASSIGN);
                }
                self.out.push(';');
            }
            StmtKind::Assign(place, op, value) => {
                self.expr(place, POSTFIX);
                self.out.push(' ');
                self.out.push_str(op);
                self.out.push(' ');
                self.expr(value, ASSIGN);
                self.out.push(';');
            }
            StmtKind::Expr(value) => {
                // An object or a function at a statement's start is read as
                // a block or a declaration.
                let grouped = starts_ambiguously(value);
                if grouped {
                    self.out.push('(');
                }
                self.expr(value, 0);
                if grouped {
                    self.out.push(')');
                }
                self.out.push(';');
            }
            StmtKind::If {
                condition,
                then,
                otherwise,
            } => {
                self.out.push_str("if (");
                self.expr(condition, 0);
                self.out.push_str(") ");
                self.block(then);
                if let Some(arm) = otherwise {
                    self.else_arm(arm);
                }
            }
            StmtKind::ForOf {
                binding,
                iterable,
                body,
            } => {
                self.out.push_str("for (const ");
                self.out.push_str(binding);
                self.out.push_str(" of ");
                self.expr(iterable, ASSIGN);
                self.out.push_str(") ");
                self.block(body);
            }
            StmtKind::While { condition, body } => {
                self.out.push_str("while (");
                self.expr(condition, 0);
                self.out.push_str(") ");
                self.block(body);
            }
            StmtKind::Return(value) => {
                self.out.push_str("return");
                if let Some(value) = value {
                    self.out.push(' ');
                    self.expr(value, 0);
                }
                self.out.push(';');
            }
            StmtKind::Throw(value) => {
                self.out.push_str("throw ");
                self.expr(value, 0);
                self.out.push(';');
            }
            StmtKind::Break => self.out.push_str("break;"),
            StmtKind::Continue => self.out.push_str("continue;"),
            StmtKind::Try {
                body,
                catch,
                finally,
            } => {
                self.out.push_str("try ");
                self.block(body);
                if let Some((arm, binding)) = catch {
                    self.clause(arm.caption.as_ref());
                    match binding {
                        Some(binding) => {
                            self.out.push_str("catch (");
                            self.out.push_str(binding);
                            self.out.push_str(") ");
                        }
                        None => self.out.push_str("catch "),
                    }
                    self.block(&arm.body);
                }
                if let Some(arm) = finally {
                    self.clause(arm.caption.as_ref());
                    self.out.push_str("finally ");
                    self.block(&arm.body);
                }
            }
            StmtKind::Function { name, function } => {
                if function.is_async {
                    self.out.push_str("async ");
                }
                self.out.push_str("function ");
                self.out.push_str(name);
                self.params(&function.params);
                if let Some(result) = &function.result {
                    self.out.push_str(": ");
                    if function.is_async {
                        self.ty(&Type::Promise(Box::new(result.clone())));
                    } else {
                        self.ty(result);
                    }
                }
                self.out.push(' ');
                match &function.body {
                    Body::Block(body) => self.block(body),
                    Body::Expr(value) => self.block(&[Stmt {
                        caption: None,
                        kind: StmtKind::Return(Some(value.clone())),
                    }]),
                }
            }
        }
    }

    /// The space or line, and the caption, before a clause keyword.
    fn clause(&mut self, caption: Option<&Caption>) {
        match caption {
            Some(caption) => {
                self.out.push('\n');
                self.pad();
                self.caption(Some(caption));
            }
            None => self.out.push(' '),
        }
    }

    fn else_arm(&mut self, arm: &Arm) {
        // `else if`: the inner `if`'s caption goes before `else`.
        if arm.caption.is_none()
            && let [
                Stmt {
                    caption,
                    kind: kind @ StmtKind::If { .. },
                },
            ] = arm.body.as_slice()
        {
            self.clause(caption.as_ref());
            self.out.push_str("else ");
            self.statement_kind(kind);
            return;
        }
        self.clause(arm.caption.as_ref());
        self.out.push_str("else ");
        self.block(&arm.body);
    }

    fn expr(&mut self, expr: &Expr, minimum: u8) {
        let grouped = precedence(expr) < minimum;
        if grouped {
            self.out.push('(');
        }
        self.expr_inner(expr);
        if grouped {
            self.out.push(')');
        }
    }

    fn expr_inner(&mut self, expr: &Expr) {
        match expr {
            Expr::Number(text) => self.out.push_str(text),
            Expr::Text(text) => self.out.push_str(&text_literal(text)),
            Expr::Bool(value) => self.out.push_str(if *value { "true" } else { "false" }),
            Expr::Null => self.out.push_str("null"),
            Expr::Undefined => self.out.push_str("undefined"),
            Expr::Ident(name) => self.out.push_str(name),
            Expr::Field(target, field) => {
                self.member_target(target);
                if crate::printer::property_identifier(field) {
                    self.out.push('.');
                    self.out.push_str(field);
                } else {
                    self.out.push('[');
                    self.out.push_str(&text_literal(field));
                    self.out.push(']');
                }
            }
            Expr::Index(target, index) => {
                self.member_target(target);
                self.out.push('[');
                self.expr(index, 0);
                self.out.push(']');
            }
            Expr::Call(callee, args) => {
                self.member_target(callee);
                self.arguments(args);
            }
            Expr::New(callee, args) => {
                self.out.push_str("new ");
                self.expr(callee, MEMBER);
                self.arguments(args);
            }
            Expr::Unary(op, value) => {
                self.out.push_str(op);
                // `- -x` and `!-x` read as written; `--x` would not.
                let spaced = *op == "-" && matches!(**value, Expr::Unary("-", _));
                if spaced {
                    self.out.push(' ');
                }
                self.expr(value, UNARY);
            }
            Expr::Binary(op, left, right) => {
                let own = binary_precedence(op);
                // `**` groups to the right; every other operator to the left.
                let (left_minimum, right_minimum) = if *op == "**" {
                    (own + 1, own)
                } else {
                    (own, own + 1)
                };
                self.expr(left, left_minimum.max(nullish_guard(op, left)));
                self.out.push(' ');
                self.out.push_str(op);
                self.out.push(' ');
                self.expr(right, right_minimum.max(nullish_guard(op, right)));
            }
            Expr::Conditional(test, then, otherwise) => {
                self.expr(test, CONDITIONAL + 1);
                self.out.push_str(" ? ");
                self.expr(then, ASSIGN);
                self.out.push_str(" : ");
                self.expr(otherwise, ASSIGN);
            }
            Expr::Array(items) => {
                self.out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        self.out.push_str(", ");
                    }
                    self.expr(item, ASSIGN);
                }
                self.out.push(']');
            }
            Expr::Object(entries) => {
                if entries.is_empty() {
                    self.out.push_str("{}");
                    return;
                }
                self.out.push_str("{ ");
                for (index, (key, value)) in entries.iter().enumerate() {
                    if index > 0 {
                        self.out.push_str(", ");
                    }
                    if matches!(value, Expr::Ident(name) if name == key) {
                        self.out.push_str(key);
                        continue;
                    }
                    self.out.push_str(&property_key(key));
                    self.out.push_str(": ");
                    self.expr(value, ASSIGN);
                }
                self.out.push_str(" }");
            }
            Expr::Template(parts) => {
                self.out.push('`');
                for part in parts {
                    match part {
                        Part::Text(text) => self.out.push_str(&template_text(text)),
                        Part::Expr(value) => {
                            self.out.push_str("${");
                            self.expr(value, 0);
                            self.out.push('}');
                        }
                    }
                }
                self.out.push('`');
            }
            Expr::Arrow(function) => {
                if function.is_async {
                    self.out.push_str("async ");
                }
                self.params(&function.params);
                if let Some(result) = &function.result {
                    self.out.push_str(": ");
                    if function.is_async {
                        self.ty(&Type::Promise(Box::new(result.clone())));
                    } else {
                        self.ty(result);
                    }
                }
                self.out.push_str(" => ");
                match &function.body {
                    Body::Expr(value) => {
                        let grouped = matches!(value, Expr::Object(_));
                        if grouped {
                            self.out.push('(');
                        }
                        self.expr(value, ASSIGN);
                        if grouped {
                            self.out.push(')');
                        }
                    }
                    Body::Block(body) => self.block(body),
                }
            }
        }
    }

    /// The object of a member access or the callee of a call: a number
    /// literal is grouped so its `.` is no decimal point.
    fn member_target(&mut self, target: &Expr) {
        if matches!(target, Expr::Number(_)) {
            self.out.push('(');
            self.expr_inner(target);
            self.out.push(')');
        } else {
            self.expr(target, MEMBER);
        }
    }

    fn arguments(&mut self, args: &[Expr]) {
        self.out.push('(');
        for (index, arg) in args.iter().enumerate() {
            if index > 0 {
                self.out.push_str(", ");
            }
            self.expr(arg, ASSIGN);
        }
        self.out.push(')');
    }
}

impl Type {
    fn has_caption(&self) -> bool {
        match self {
            Self::Word(_) | Self::Literal(_) => false,
            Self::Array(item) | Self::Promise(item) => item.has_caption(),
            Self::Union(members) => members.iter().any(Self::has_caption),
            Self::Object(properties, rest) => {
                rest.as_ref().is_some_and(|rest| rest.has_caption())
                    || properties
                        .iter()
                        .any(|property| property.caption.is_some() || property.ty.has_caption())
            }
        }
    }
}

const ASSIGN: u8 = 2;
const CONDITIONAL: u8 = 3;
const UNARY: u8 = 15;
const POSTFIX: u8 = 17;
const MEMBER: u8 = 18;

fn binary_precedence(op: &str) -> u8 {
    match op {
        "??" | "||" => 4,
        "&&" => 5,
        "|" => 6,
        "^" => 7,
        "&" => 8,
        "==" | "!=" | "===" | "!==" => 9,
        "<" | ">" | "<=" | ">=" | "instanceof" | "in" => 10,
        "<<" | ">>" | ">>>" => 11,
        "+" | "-" => 12,
        "*" | "/" | "%" => 13,
        "**" => 14,
        _ => 0,
    }
}

/// `??` never mixes with `&&` or `||` unparenthesized.
fn nullish_guard(op: &str, operand: &Expr) -> u8 {
    let logical = |op: &str| matches!(op, "&&" | "||");
    match operand {
        Expr::Binary(inner, ..)
            if (op == "??" && logical(inner)) || (logical(op) && *inner == "??") =>
        {
            u8::MAX
        }
        _ => 0,
    }
}

fn precedence(expr: &Expr) -> u8 {
    match expr {
        Expr::Arrow(_) => ASSIGN,
        Expr::Conditional(..) => CONDITIONAL,
        Expr::Binary(op, ..) => binary_precedence(op),
        Expr::Unary(..) => UNARY,
        Expr::Number(text) if text.starts_with('-') => UNARY,
        Expr::Call(..) | Expr::New(..) => POSTFIX,
        _ => 20,
    }
}

fn starts_ambiguously(expr: &Expr) -> bool {
    match expr {
        Expr::Object(_) | Expr::Arrow(_) => true,
        Expr::Field(target, _) | Expr::Index(target, _) | Expr::Call(target, _) => {
            starts_ambiguously(target)
        }
        Expr::Binary(_, left, _) => starts_ambiguously(left),
        Expr::Conditional(test, ..) => starts_ambiguously(test),
        _ => false,
    }
}

/// A text literal TypeScript reads back to `text`.
pub(super) fn text_literal(text: &str) -> String {
    serde_json::Value::from(text).to_string()
}

fn property_key(key: &str) -> String {
    if crate::printer::property_identifier(key) {
        key.to_owned()
    } else {
        text_literal(key)
    }
}

fn template_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '`' => out.push_str("\\`"),
            '\\' => out.push_str("\\\\"),
            '$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
            '\r' => out.push_str("\\r"),
            c if c < ' ' && c != '\n' && c != '\t' => {
                out.push_str(&format!("\\u{{{:x}}}", u32::from(c)));
            }
            c => out.push(c),
        }
    }
    out
}
