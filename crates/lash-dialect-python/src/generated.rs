//! Direct decoder of Python generated-source v1; no authored lowering or library.
//! The paired generic rule inventory is docs/design/generated-source-python.md.
use crate::GeneratedSourceCode as Code;
use lash_kernel_dialect::{Diagnostic, GeneratedSourceLimits, PrintError, check_document_depth};
use lash_kernel_doc as k;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
mod metadata;

fn error(code: Code, message: impl Into<String>) -> Diagnostic {
    Diagnostic {
        code: code.as_str().into(),
        message: message.into(),
        span: None,
        kind: if code == Code::InvalidGeneratedSource {
            lash_kernel_dialect::DiagnosticKind::ProgramDefect
        } else {
            lash_kernel_dialect::DiagnosticKind::Refusal
        },
        repairs: vec![],
    }
}
type Result<T> = std::result::Result<T, Diagnostic>;
fn invalid(message: impl Into<String>) -> Diagnostic {
    error(Code::InvalidGeneratedSource, message)
}
fn format_error(message: impl Into<String>) -> Diagnostic {
    error(Code::GeneratedSourceFormat, message)
}
fn depth_error() -> Diagnostic {
    error(
        Code::GeneratedSourceDepth,
        "generated source exceeds depth budget",
    )
}
/// Reconstruct an unadmitted Document, without an installed helper catalogue.
/// Only the explicit Python v1 envelope and its paired exact forms are accepted.
pub fn compile_generated(source: &str, limits: GeneratedSourceLimits) -> Result<k::Document> {
    if source.len() > limits.max_bytes {
        return Err(error(
            Code::GeneratedSourceBytes,
            "generated source exceeds byte budget",
        ));
    }
    // A fixed bounded stack covers the kernel's 64-level native ceiling and
    // preflighted JSON metadata. Parsing and destruction remain on this stack.
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("kernel-python-decode".into())
            .stack_size(8 * 1024 * 1024)
            .spawn_scoped(scope, || {
                let document = Reader::new(source, limits)?.document()?;
                check_document_depth(&document, limits).map_err(|e| match e {
                    PrintError::Bytes { .. } => error(Code::GeneratedSourceBytes, e.to_string()),
                    PrintError::Depth { .. } => depth_error(),
                    _ => invalid(e.to_string()),
                })?;
                Ok(document)
            })
            .map_err(|e| invalid(e.to_string()))?;
        handle
            .join()
            .unwrap_or_else(|_| Err(invalid("generated-source decoder failed")))
    })
}

/// A cursor reads only the small generic operand grammar, never a Python AST.
/// Cursor primitives do not recurse: metadata arrays contain strings, aliases or atoms, and action
/// calls have exactly one callee level. Unexpected nested syntax refuses here.
struct Cursor<'a> {
    text: &'a str,
}
impl<'a> Cursor<'a> {
    fn new(text: &'a str) -> Self {
        Self { text }
    }
    fn take(&mut self, token: &str) -> bool {
        self.text = self.text.trim_start_matches(' ');
        if let Some(rest) = self.text.strip_prefix(token) {
            self.text = rest;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, token: &str) -> Result<()> {
        if self.take(token) {
            Ok(())
        } else {
            Err(invalid(format!("expected {token}")))
        }
    }
    fn word(&mut self) -> Result<&'a str> {
        self.text = self.text.trim_start_matches(' ');
        let end = self
            .text
            .char_indices()
            .find(|(_, c)| !unicode_ident::is_xid_continue(*c))
            .map_or(self.text.len(), |(i, _)| i);
        let word = &self.text[..end];
        if !crate::printer::identifier(word) {
            return Err(invalid("expected an identifier"));
        }
        self.text = &self.text[end..];
        Ok(word)
    }
    fn string(&mut self) -> Result<String> {
        self.text = self.text.trim_start_matches(' ');
        if !self.text.starts_with('"') {
            return Err(invalid("expected JSON-quoted Python text"));
        }
        let mut escaped = false;
        for (i, c) in self.text.char_indices().skip(1) {
            if escaped {
                escaped = false;
                continue;
            }
            if c == '\\' {
                escaped = true;
            } else if c == '"' {
                let text = serde_json::from_str(&self.text[..i + 1])
                    .map_err(|e| invalid(e.to_string()))?;
                self.text = &self.text[i + 1..];
                return Ok(text);
            }
        }
        Err(invalid("unterminated text"))
    }
    fn operation(&mut self) -> Result<&'a str> {
        if self.word()? != "k" {
            return Err(invalid("expected reserved k namespace"));
        }
        self.expect(".")?;
        let name = self.word()?;
        self.expect("(")?;
        Ok(name)
    }
    fn end(&mut self, suite: bool) -> Result<()> {
        self.expect(")")?;
        if suite {
            self.expect(":")?;
        }
        self.finish()
    }
    fn finish(&self) -> Result<()> {
        if self.text.trim().is_empty() {
            Ok(())
        } else {
            Err(invalid("trailing unconsumed operands"))
        }
    }
    fn strings(&mut self) -> Result<Vec<String>> {
        self.expect("[")?;
        let mut items = vec![];
        if self.take("]") {
            return Ok(items);
        }
        loop {
            items.push(self.string()?);
            if self.take("]") {
                return Ok(items);
            }
            self.expect(",")?;
        }
    }
}
#[derive(Clone, Copy)]
struct Line<'a> {
    indent: usize,
    text: &'a str,
}
struct Reader<'a> {
    lines: Vec<Line<'a>>,
    next: usize,
    limit: usize,
    name_bytes: Cell<usize>,
    byte_limit: usize,
    names: BTreeMap<String, k::Name>,
    helpers: BTreeMap<String, k::FunctionId>,
}
impl<'a> Reader<'a> {
    fn new(source: &'a str, limits: GeneratedSourceLimits) -> Result<Self> {
        let limit = limits.max_depth.min(64);
        let mut lines = vec![];
        for line in source.lines() {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let indent = line.bytes().take_while(|b| *b == b' ').count();
            if indent % 4 != 0 || line.as_bytes().get(indent) == Some(&b'\t') {
                return Err(invalid("generic suites use four-space indentation"));
            }
            // One declaration wrapper precedes the root block. Every other
            // indentation step is a real Node child, not an auxiliary tree.
            if indent / 4 > limit + 2 {
                return Err(depth_error());
            }
            lines.push(Line {
                indent: indent / 4,
                text: line[indent..].trim_end(),
            });
        }
        Ok(Self {
            lines,
            next: 0,
            limit,
            name_bytes: Cell::new(0),
            byte_limit: limits.max_bytes,
            names: BTreeMap::new(),
            helpers: BTreeMap::new(),
        })
    }
    fn line(&mut self, indent: usize) -> Result<(Cursor<'a>, bool)> {
        let line = *self
            .lines
            .get(self.next)
            .ok_or_else(|| invalid("missing kernel node"))?;
        if line.indent != indent {
            return Err(invalid("unexpected indentation"));
        }
        self.next += 1;
        let (text, suite) = line
            .text
            .strip_prefix("with ")
            .map_or((line.text, false), |s| (s, true));
        Ok((Cursor::new(text), suite))
    }
    fn depth(&self, depth: usize) -> Result<()> {
        if depth > self.limit {
            Err(depth_error())
        } else {
            Ok(())
        }
    }
    fn more(&self, indent: usize) -> bool {
        self.lines
            .get(self.next)
            .is_some_and(|l| l.indent == indent)
    }
    fn closed(&self, indent: usize) -> Result<()> {
        if self.lines.get(self.next).is_some_and(|l| l.indent > indent) {
            Err(invalid("extra kernel operands in suite"))
        } else {
            Ok(())
        }
    }
    fn empty(&mut self, indent: usize) -> bool {
        if self
            .lines
            .get(self.next)
            .is_some_and(|l| l.indent == indent && l.text == "pass")
        {
            self.next += 1;
            true
        } else {
            false
        }
    }
    fn named(&self, alias: &str) -> Result<k::Name> {
        let name = self
            .names
            .get(alias)
            .ok_or_else(|| invalid("undeclared identifier alias"))?;
        // Alias compression can multiply a large original string. Bound the
        // total bytes of reconstructed Names before cloning, not after a whole
        // Document has already been allocated. Canonical bytes contain at least
        // these bytes, so this lower bound never rejects an in-budget document.
        let next = self
            .name_bytes
            .get()
            .checked_add(name.as_str().len())
            .filter(|n| *n <= self.byte_limit)
            .ok_or_else(|| {
                error(
                    Code::GeneratedSourceBytes,
                    "decoded names exceed byte budget",
                )
            })?;
        self.name_bytes.set(next);
        Ok(name.clone())
    }
    fn name(&self, c: &mut Cursor<'_>) -> Result<k::Name> {
        self.named(c.word()?)
    }
    fn id(&self, c: &mut Cursor<'_>) -> Result<k::FunctionId> {
        self.helpers
            .get(c.word()?)
            .copied()
            .ok_or_else(|| invalid("undeclared helper alias"))
    }
    fn names(&self, c: &mut Cursor<'_>) -> Result<Vec<k::Name>> {
        c.expect("[")?;
        let mut names = vec![];
        if c.take("]") {
            return Ok(names);
        }
        loop {
            names.push(self.name(c)?);
            if c.take("]") {
                return Ok(names);
            }
            c.expect(",")?;
        }
    }
    fn document(mut self) -> Result<k::Document> {
        let (mut c, suite) = self.line(0).map_err(|_| {
            format_error("missing generated-source v1 envelope; re-export the Document")
        })?;
        let envelope = (|| -> Result<k::Document> {
            if suite || c.operation()? != "document" {
                return Err(format_error("missing generated-source v1 envelope"));
            }
            if c.string()? != "lash.generated.python" {
                return Err(format_error("wrong generated-source dialect"));
            }
            c.expect(",")?;
            if c.string()? != "1" {
                return Err(format_error("unsupported generated-source version"));
            }
            c.expect(",")?;
            let manifest: k::Manifest = decode(&c.string()?)?;
            if k::KernelVersion::of(manifest.kernel).is_none() {
                return Err(format_error("unsupported kernel version"));
            }
            c.expect(",")?;
            let entries = decode(&c.string()?)?;
            c.expect(",")?;
            let mut private_bindings = BTreeSet::new();
            for n in c.strings()? {
                if !private_bindings.insert(k::Name::new(n)) {
                    return Err(format_error("duplicate private binding"));
                }
            }
            c.expect(",")?;
            let aliases: Vec<(String, k::Name)> = decode(&c.string()?)?;
            let mut originals = BTreeSet::new();
            for (alias, name) in aliases {
                let legal = crate::printer::legal_identifier(&alias)
                    || alias
                        .strip_prefix("kernel_name_")
                        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
                if !legal
                    || !originals.insert(name.clone())
                    || self.names.insert(alias, name).is_some()
                {
                    return Err(format_error(
                        "duplicate, reserved or contradictory identifier alias",
                    ));
                }
            }
            c.expect(",")?;
            let aliases: Vec<(String, k::FunctionId)> = decode(&c.string()?)?;
            let mut ids = BTreeSet::new();
            for (alias, id) in aliases {
                if !alias
                    .strip_prefix("helper_")
                    .is_some_and(|s| !s.is_empty() && s.chars().all(unicode_ident::is_xid_continue))
                    || !ids.insert(id)
                    || self.helpers.insert(alias, id).is_some()
                {
                    return Err(format_error("duplicate or malformed helper alias"));
                }
            }
            c.end(false)?;
            Ok(k::Document {
                manifest,
                entries,
                private_bindings,
                functions: BTreeMap::new(),
                main: vec![],
            })
        })()
        .map_err(|mut e| {
            if e.code != Code::GeneratedSourceDepth.as_str() {
                e.code = Code::GeneratedSourceFormat.as_str().into();
            }
            e
        })?;
        let mut document = envelope;
        let mut main = false;
        while self.more(0) {
            let (mut c, suite) = self.line(0)?;
            if !suite || main {
                return Err(format_error(
                    "expected declarations followed by exactly one main",
                ));
            }
            match c.operation()? {
                "declare" => {
                    let name = k::Name::new(c.string()?);
                    c.expect(",")?;
                    let params = self.names(&mut c)?;
                    c.end(true)?;
                    let body = self.block(1, 0)?;
                    self.closed(0)?;
                    if document
                        .functions
                        .insert(name, k::Function { params, body })
                        .is_some()
                    {
                        return Err(format_error("duplicate declaration"));
                    }
                }
                "main" => {
                    c.end(true)?;
                    document.main = self.block(1, 0)?;
                    self.closed(0)?;
                    main = true;
                }
                _ => return Err(format_error("unknown generated module item")),
            }
        }
        if !main || self.next != self.lines.len() {
            return Err(format_error("missing main or trailing program text"));
        }
        Ok(document)
    }
    fn block(&mut self, indent: usize, depth: usize) -> Result<k::Block> {
        self.depth(depth)?;
        let (mut c, suite) = self.line(indent)?;
        if !suite || c.operation()? != "block" {
            return Err(invalid("expected explicit block"));
        }
        c.end(true)?;
        let mut body = vec![];
        if !self.empty(indent + 1) {
            if !self.more(indent + 1) {
                return Err(invalid("empty blocks require pass"));
            }
            while self.more(indent + 1) {
                body.push(self.statement(indent + 1, depth + 1)?);
            }
        }
        self.closed(indent)?;
        Ok(body)
    }
    fn statement(&mut self, indent: usize, depth: usize) -> Result<k::Stmt> {
        self.depth(depth)?;
        let (mut c, suite) = self.line(indent)?;
        if !suite {
            return self.readable_statement(c, indent, depth);
        }
        let op = c.operation()?;
        let i = indent + 1;
        let d = depth + 1;
        let stmt = match op {
            "let" | "assign" => {
                let name = self.name(&mut c)?;
                c.end(true)?;
                let value = self.rhs(i, d)?;
                if op == "let" {
                    k::Stmt::Let { name, value }
                } else {
                    k::Stmt::Assign {
                        place: k::Place::Variable(name),
                        value,
                    }
                }
            }
            "assign_field" | "assign_index" | "remove_field" | "remove_index" => {
                let member = self.member(op.ends_with("field"), &mut c, i, d)?;
                if op.starts_with("assign_") {
                    k::Stmt::Assign {
                        place: k::Place::Member(member),
                        value: self.rhs(i, d)?,
                    }
                } else {
                    k::Stmt::Remove { member }
                }
            }
            "do" => {
                c.end(true)?;
                k::Stmt::Do {
                    action: self.action_line(i, d)?,
                }
            }
            "if_" => {
                c.end(true)?;
                k::Stmt::If {
                    condition: self.expr(i, d)?,
                    then_block: self.block(i, d)?,
                    else_block: self.block(i, d)?,
                }
            }
            "for_" => {
                let binding = self.name(&mut c)?;
                c.end(true)?;
                k::Stmt::For {
                    binding,
                    iterable: self.expr(i, d)?,
                    body: self.block(i, d)?,
                }
            }
            "while_" => {
                c.end(true)?;
                k::Stmt::While {
                    condition: self.expr(i, d)?,
                    body: self.block(i, d)?,
                }
            }
            "return_" | "throw" | "print" | "finish" | "fail" => {
                c.end(true)?;
                let value = self.expr(i, d)?;
                match op {
                    "return_" => k::Stmt::Return { value },
                    "throw" => k::Stmt::Throw { value },
                    "print" => k::Stmt::Print { value },
                    "finish" => k::Stmt::Finish { value },
                    _ => k::Stmt::Fail { value },
                }
            }
            "try_" => {
                let alias = c.word()?;
                let binding = if alias == "None" {
                    None
                } else {
                    Some(self.named(alias)?)
                };
                c.expect(",")?;
                let finally = match c.word()? {
                    "True" => true,
                    "False" => false,
                    _ => return Err(invalid("expected finally presence flag")),
                };
                c.end(true)?;
                let body = self.block(i, d)?;
                let catch = binding
                    .map(|binding| self.block(i, d).map(|body| k::Catch { binding, body }))
                    .transpose()?;
                let finally = if finally {
                    Some(self.block(i, d)?)
                } else {
                    None
                };
                k::Stmt::Try(k::TryStmt {
                    body,
                    catch,
                    finally,
                })
            }
            _ => return Err(invalid("unknown statement form")),
        };
        self.closed(indent)?;
        Ok(stmt)
    }
    fn starts(&self, indent: usize, prefix: &str) -> bool {
        self.lines
            .get(self.next)
            .is_some_and(|l| l.indent == indent && l.text.starts_with(prefix))
    }
    fn readable_block(&mut self, indent: usize, depth: usize) -> Result<k::Block> {
        self.depth(depth)?;
        let mut body = vec![];
        if !self.empty(indent) {
            if !self.more(indent) {
                return Err(invalid("empty suites require pass"));
            }
            while self.more(indent) {
                body.push(self.statement(indent, depth + 1)?);
            }
        }
        self.closed(indent - 1)?;
        Ok(body)
    }
    fn readable_if(&mut self, mut c: Cursor<'_>, indent: usize, depth: usize) -> Result<k::Stmt> {
        self.depth(depth)?;
        let condition = self.inline(&mut c, depth + 1)?;
        c.expect(":")?;
        c.finish()?;
        let then_block = self.readable_block(indent + 1, depth + 1)?;
        let else_block = if self.starts(indent, "elif ") {
            // Exact inverse: elif is one If statement inside the else Block.
            self.depth(depth + 1)?;
            let (mut c, _) = self.line(indent)?;
            c.expect("elif ")?;
            vec![self.readable_if(c, indent, depth + 2)?]
        } else {
            let (mut c, suite) = self.line(indent)?;
            if suite {
                return Err(invalid("expected else clause"));
            }
            c.expect("else:")?;
            c.finish()?;
            self.readable_block(indent + 1, depth + 1)?
        };
        Ok(k::Stmt::If {
            condition,
            then_block,
            else_block,
        })
    }
    fn readable_statement(
        &mut self,
        mut c: Cursor<'_>,
        indent: usize,
        depth: usize,
    ) -> Result<k::Stmt> {
        let stmt = if c.take("if ") {
            self.readable_if(c, indent, depth)?
        } else if c.take("for ") {
            let binding = self.name(&mut c)?;
            c.expect("in ")?;
            let iterable = self.inline(&mut c, depth + 1)?;
            c.expect(":")?;
            c.finish()?;
            k::Stmt::For {
                binding,
                iterable,
                body: self.readable_block(indent + 1, depth + 1)?,
            }
        } else if c.take("while ") {
            let condition = self.inline(&mut c, depth + 1)?;
            c.expect(":")?;
            c.finish()?;
            k::Stmt::While {
                condition,
                body: self.readable_block(indent + 1, depth + 1)?,
            }
        } else if c.take("try:") {
            c.finish()?;
            let body = self.readable_block(indent + 1, depth + 1)?;
            let catch = if self.starts(indent, "except ") {
                let (mut c, suite) = self.line(indent)?;
                if suite {
                    return Err(invalid("expected except clause"));
                }
                c.expect("except BaseException as ")?;
                let binding = self.name(&mut c)?;
                c.expect(":")?;
                c.finish()?;
                Some(k::Catch {
                    binding,
                    body: self.readable_block(indent + 1, depth + 1)?,
                })
            } else {
                None
            };
            let finally = if self.starts(indent, "finally:") {
                let (mut c, _) = self.line(indent)?;
                c.expect("finally:")?;
                c.finish()?;
                Some(self.readable_block(indent + 1, depth + 1)?)
            } else {
                None
            };
            if catch.is_none() && finally.is_none() {
                return Err(invalid("try requires a handler"));
            }
            k::Stmt::Try(k::TryStmt {
                body,
                catch,
                finally,
            })
        } else if c.take("return ") {
            let value = self.inline(&mut c, depth + 1)?;
            c.finish()?;
            k::Stmt::Return { value }
        } else if c.take("raise ") {
            let value = self.inline(&mut c, depth + 1)?;
            c.finish()?;
            k::Stmt::Throw { value }
        } else if c.text == "break" || c.text == "continue" {
            if c.text == "break" {
                k::Stmt::Break
            } else {
                k::Stmt::Continue
            }
        } else {
            let op = c.operation()?;
            let stmt = match op {
                "break_" => k::Stmt::Break,
                "continue_" => k::Stmt::Continue,
                "print" | "finish" | "fail" => {
                    let value = self.inline(&mut c, depth + 1)?;
                    match op {
                        "print" => k::Stmt::Print { value },
                        "finish" => k::Stmt::Finish { value },
                        _ => k::Stmt::Fail { value },
                    }
                }
                _ => return Err(invalid("unknown readable statement")),
            };
            c.end(false)?;
            stmt
        };
        self.closed(indent)?;
        Ok(stmt)
    }
    fn inline(&self, c: &mut Cursor<'_>, depth: usize) -> Result<k::Expr> {
        self.depth(depth)?;
        let (expr, height) = self.inline_value(c, 0)?;
        self.depth(depth + height - 1)?;
        Ok(expr)
    }
    // Parentheses create no Node. Bound syntax recursion separately and compute
    // real tree height as postfixes are wrapped, before allocating deep trees.
    fn inline_value(&self, c: &mut Cursor<'_>, nesting: usize) -> Result<(k::Expr, usize)> {
        if nesting > self.limit * 2 + 2 {
            return Err(depth_error());
        }
        let (mut expr, mut height) = if c.take("(") {
            let value = self.inline_value(c, nesting + 1)?;
            c.expect(")")?;
            value
        } else if c.text.trim_start().starts_with("helper_") {
            let function = self.id(c)?;
            c.expect("(")?;
            let mut args = vec![];
            let mut height = 1;
            if !c.take(")") {
                loop {
                    let (arg, h) = self.inline_value(c, nesting + 1)?;
                    height = height.max(h + 1);
                    self.depth(height - 1)?;
                    args.push(arg);
                    if c.take(")") {
                        break;
                    }
                    c.expect(",")?;
                }
            }
            (k::Expr::Call { function, args }, height)
        } else if c.take("k.clock()") {
            (k::Expr::Clock, 1)
        } else if c.take("k.random()") {
            (k::Expr::Random, 1)
        } else {
            let atom = self.atom(c)?;
            (
                match atom {
                    k::Atom::Variable(n) => k::Expr::Variable(n),
                    k::Atom::Literal(l) => k::Expr::Literal(l),
                },
                1,
            )
        };
        loop {
            let member = if c.take(".") {
                let field = c.word()?;
                if !crate::printer::legal_identifier(field) {
                    return Err(invalid("reserved attribute spelling"));
                }
                self.depth(height)?;
                k::Member::Field {
                    target: expr,
                    field: field.into(),
                }
            } else if c.take("[") {
                let (index, h) = self.inline_value(c, nesting + 1)?;
                c.expect("]")?;
                height = height.max(h);
                self.depth(height)?;
                k::Member::Index {
                    target: expr,
                    index,
                }
            } else {
                break;
            };
            height += 1;
            expr = k::Expr::Member(Box::new(member));
        }
        Ok((expr, height))
    }
    fn member(
        &mut self,
        field: bool,
        c: &mut Cursor<'_>,
        indent: usize,
        depth: usize,
    ) -> Result<k::Member> {
        let field = if field { Some(c.string()?) } else { None };
        c.end(true)?;
        let target = self.expr(indent, depth)?;
        Ok(if let Some(field) = field {
            k::Member::Field { target, field }
        } else {
            k::Member::Index {
                target,
                index: self.expr(indent, depth)?,
            }
        })
    }
    fn rhs(&mut self, indent: usize, depth: usize) -> Result<k::Rhs> {
        let line = self
            .lines
            .get(self.next)
            .ok_or_else(|| invalid("missing right-hand side"))?;
        let mut c = Cursor::new(line.text);
        let action = c.operation().is_ok_and(|op| {
            matches!(
                op,
                "call" | "spawn" | "perform" | "sleep" | "join" | "join_many" | "cancel" | "yield_"
            )
        });
        if action {
            self.action_line(indent, depth).map(k::Rhs::Action)
        } else {
            self.expr(indent, depth).map(k::Rhs::Expr)
        }
    }
    fn expr(&mut self, indent: usize, depth: usize) -> Result<k::Expr> {
        self.depth(depth)?;
        let (mut c, suite) = self.line(indent)?;
        if !suite {
            let expr = self.inline(&mut c, depth)?;
            c.finish()?;
            return Ok(expr);
        }
        let op = c.operation()?;
        let i = indent + 1;
        let d = depth + 1;
        let expr = match op {
            "field" | "index" => {
                k::Expr::Member(Box::new(self.member(op == "field", &mut c, i, d)?))
            }
            "closure" => {
                let params = self.names(&mut c)?;
                c.end(true)?;
                k::Expr::Closure(Box::new(k::Closure {
                    params,
                    body: self.block(i, d)?,
                }))
            }
            "read" => {
                c.end(true)?;
                k::Expr::Read(Box::new(k::ProjectionRead {
                    handle: self.expr(i, d)?,
                    request: self.expr(i, d)?,
                }))
            }
            "tuple" | "list" | "set" | "map" | "record" | "invoke" => {
                let fields = if op == "record" { c.strings()? } else { vec![] };
                let id = if op == "invoke" {
                    Some(self.id(&mut c)?)
                } else {
                    None
                };
                c.end(true)?;
                let mut items = vec![];
                if !self.empty(i) {
                    if !self.more(i) {
                        return Err(invalid("empty expression suites require pass"));
                    }
                    while self.more(i) {
                        items.push(self.expr(i, d)?);
                    }
                }
                match op {
                    "tuple" => k::Expr::Tuple(items),
                    "list" => k::Expr::List(items),
                    "set" => k::Expr::Set(items),
                    "map" => {
                        if items.len() % 2 != 0 {
                            return Err(invalid("map requires key/value pairs"));
                        }
                        let mut entries = vec![];
                        let mut items = items.into_iter();
                        while let Some(key) = items.next() {
                            entries.push(k::MapEntry {
                                key,
                                value: items.next().ok_or_else(|| invalid("missing map value"))?,
                            });
                        }
                        k::Expr::Map(entries)
                    }
                    "record" => {
                        if fields.len() != items.len() {
                            return Err(invalid("record fields and values differ in count"));
                        }
                        k::Expr::Record(
                            fields
                                .into_iter()
                                .zip(items)
                                .map(|(field, value)| k::RecordEntry { field, value })
                                .collect(),
                        )
                    }
                    _ => k::Expr::Call {
                        function: id.ok_or_else(|| invalid("missing helper identity"))?,
                        args: items,
                    },
                }
            }
            _ => return Err(invalid("unknown expression form")),
        };
        self.closed(indent)?;
        Ok(expr)
    }
    fn atom(&self, c: &mut Cursor<'_>) -> Result<k::Atom> {
        if c.text.trim_start().starts_with('"') {
            return Ok(k::Atom::Literal(k::Literal::Text(c.string()?)));
        }
        c.text = c.text.trim_start_matches(' ');
        if c.text
            .starts_with(|ch: char| ch.is_ascii_digit() || ch == '-')
        {
            let end = c
                .text
                .find(|ch: char| !ch.is_ascii_digit() && !matches!(ch, '.' | 'e' | '-'))
                .unwrap_or(c.text.len());
            let token = &c.text[..end];
            c.text = &c.text[end..];
            let literal = if token.contains(['.', 'e']) {
                let n = k::Float::parse(token).map_err(|e| invalid(e.to_string()))?;
                if !n.get().is_finite() {
                    return Err(invalid("non-finite native number"));
                }
                k::Literal::Float(n)
            } else {
                k::Literal::Int(k::Integer::parse(token).map_err(|e| invalid(e.to_string()))?)
            };
            return Ok(k::Atom::Literal(literal));
        }
        let word = c.word()?;
        let lit = match word {
            "None" => k::Literal::Null,
            "True" => k::Literal::Bool(true),
            "False" => k::Literal::Bool(false),
            "k" => {
                c.expect(".")?;
                let op = c.word()?;
                if op == "absent" {
                    return Ok(k::Atom::Literal(k::Literal::Absent));
                }
                c.expect("(")?;
                let value = c.string()?;
                c.expect(")")?;
                match op {
                    "int" => k::Literal::Int(
                        k::Integer::parse(&value).map_err(|e| invalid(e.to_string()))?,
                    ),
                    "float" => k::Literal::Float(
                        k::Float::parse(&value).map_err(|e| invalid(e.to_string()))?,
                    ),
                    "bytes" => k::Literal::Bytes(
                        k::Bytes::parse_hex(&value).map_err(|e| invalid(e.to_string()))?,
                    ),
                    "function" => k::Literal::Function(k::Name::new(value)),
                    _ => return Err(invalid("unknown exact literal")),
                }
            }
            alias => return self.named(alias).map(k::Atom::Variable),
        };
        Ok(k::Atom::Literal(lit))
    }
    fn atoms(&self, c: &mut Cursor<'_>) -> Result<Vec<k::Atom>> {
        c.expect("[")?;
        let mut atoms = vec![];
        if c.take("]") {
            return Ok(atoms);
        }
        loop {
            atoms.push(self.atom(c)?);
            if c.take("]") {
                return Ok(atoms);
            }
            c.expect(",")?;
        }
    }
    fn callee(&self, c: &mut Cursor<'_>) -> Result<k::Callee> {
        let op = c.operation()?;
        let callee = match op {
            "declared" => k::Callee::Declared(k::Name::new(c.string()?)),
            "value" => k::Callee::Value(k::Name::new(c.string()?)),
            "library" => k::Callee::Library(self.id(c)?),
            _ => return Err(invalid("unknown callee kind")),
        };
        c.expect(")")?;
        Ok(callee)
    }
    fn action_operands(&self, c: &mut Cursor<'_>) -> Result<(k::Callee, Vec<k::Atom>)> {
        if c.text.trim_start().starts_with("helper_") {
            let id = self.id(c)?;
            c.expect("(")?;
            let mut args = vec![];
            if !c.take(")") {
                loop {
                    args.push(self.atom(c)?);
                    if c.take(")") {
                        break;
                    }
                    c.expect(",")?;
                }
            }
            Ok((k::Callee::Library(id), args))
        } else {
            let callee = self.callee(c)?;
            c.expect(",")?;
            Ok((callee, self.atoms(c)?))
        }
    }
    fn action_line(&mut self, indent: usize, depth: usize) -> Result<k::Action> {
        self.depth(depth)?;
        let (mut c, suite) = self.line(indent)?;
        if suite {
            return Err(invalid("actions have only atomic operands"));
        }
        let action = match c.operation()? {
            "call" => {
                let (callee, args) = self.action_operands(&mut c)?;
                k::Action::Call { callee, args }
            }
            "spawn" => {
                let (callee, args) = self.action_operands(&mut c)?;
                k::Action::Spawn { callee, args }
            }
            "perform" => {
                let effect = k::EffectName::new(c.string()?).map_err(|e| invalid(e.to_string()))?;
                c.expect(",")?;
                let args = self.atoms(&mut c)?;
                c.expect(",")?;
                if c.operation()? != "type" {
                    return Err(invalid("expected complete result type"));
                }
                let result = decode(&c.string()?)?;
                c.expect(")")?;
                k::Action::Perform {
                    effect,
                    args,
                    result,
                }
            }
            "sleep" => k::Action::Sleep {
                duration: self.atom(&mut c)?,
            },
            "join" => k::Action::Join {
                task: self.atom(&mut c)?,
            },
            "cancel" => k::Action::Cancel {
                task: self.atom(&mut c)?,
            },
            "join_many" => {
                let mode = match c.string()?.as_str() {
                    "all" => k::JoinMode::All,
                    "all_settled" => k::JoinMode::AllSettled,
                    "race" => k::JoinMode::Race,
                    "any" => k::JoinMode::Any,
                    _ => return Err(invalid("unknown join mode")),
                };
                c.expect(",")?;
                k::Action::JoinMany {
                    mode,
                    tasks: self.atom(&mut c)?,
                }
            }
            "yield_" => k::Action::Yield,
            _ => return Err(invalid("unknown action form")),
        };
        c.end(false)?;
        Ok(action)
    }
}
fn decode<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    // Metadata JSON is bounded before either duplicate checking or serde
    // traverses it. Strings do not contribute delimiter depth.
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for b in text.bytes() {
        if quoted {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                quoted = false;
            }
        } else {
            match b {
                b'"' => quoted = true,
                b'[' | b'{' => {
                    depth += 1;
                    if depth > k::KernelDataLimits::standard().document_json_depth() {
                        return Err(depth_error());
                    }
                }
                b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    metadata::check(text).map_err(|e| format_error(e.to_string()))?;
    let mut decoder = serde_json::Deserializer::from_str(text);
    decoder.disable_recursion_limit();
    let value = T::deserialize(&mut decoder).map_err(|e| format_error(e.to_string()))?;
    decoder.end().map_err(|e| format_error(e.to_string()))?;
    Ok(value)
}
