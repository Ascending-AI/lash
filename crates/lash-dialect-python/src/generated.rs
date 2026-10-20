//! Direct decoder of Python generated-source v1; no authored lowering or library.
//! The paired generic rule inventory is docs/design/generated-source-python.md.
use crate::{GeneratedSourceCode as Code, generated_scope::BindingScope};
use lash_kernel_dialect::{
    Diagnostic, GeneratedSourceLimits, PrintError, check_document_depth, check_metadata_uniqueness,
};
use lash_kernel_doc as k;
use std::cell::Cell;
use std::collections::BTreeMap;

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
mod actions;
mod captioned;
mod inline;

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
/// Read exported Python back into the document it was printed from, or into
/// that document as edited, without an installed helper catalogue or
/// admission. The source carries only code; the manifest, entries, private
/// bindings, helper identities and name renames come from `base`.
///
/// The captions are those the source spells as `#:` lines above a
/// statement, a clause, a `def`, a parameter written on its own line or a
/// field of a `TypedDict` its annotations name ([`crate::captions`]); any
/// other comment is ignored.
pub fn read_kernel(
    source: &str,
    base: &k::Document,
    limits: GeneratedSourceLimits,
) -> Result<(k::Document, k::Captions)> {
    let (document, entries) = read_document(source, base, limits, false)?;
    let captions = k::Captions::new(&document, entries).map_err(|e| invalid(e.to_string()))?;
    Ok((document, captions))
}

/// The regions of a printed module written in kernel form, each opened by
/// the kernel marker, read against `base` (`crate::readable`).
pub(crate) struct Marked {
    /// The kernel-form functions and `main`.
    pub(crate) document: k::Document,
    pub(crate) captions: Vec<k::CaptionEntry>,
    /// Where each kernel-form region is in the source.
    pub(crate) spans: Vec<std::ops::Range<usize>>,
    /// Whether `main` is written in kernel form.
    pub(crate) main: bool,
    /// How many functions are not.
    pub(crate) unmarked: usize,
}

/// A top-level item of a printed module.
struct Item<'a> {
    /// Where it is, from its first line to the end of its last.
    span: std::ops::Range<usize>,
    /// The function it declares.
    def: Option<&'a str>,
    marked: bool,
}

/// Reads the kernel-form regions of `source`. Every other function, and
/// every import, is read as authored source by the caller; here they are
/// blank, so offsets are kept.
pub(crate) fn read_marked(
    source: &str,
    base: &k::Document,
    limits: GeneratedSourceLimits,
) -> Result<Marked> {
    let marker = format!("    \"{}\"", crate::printer::KERNEL_MARKER);
    let mut items: Vec<Item<'_>> = Vec::new();
    // Where the comment lines right above the line read next begin: an
    // item's captions are part of it.
    let mut comments = None;
    for (start, line) in captioned::lines(source) {
        if line.trim().is_empty() {
            comments = None;
            continue;
        }
        if line.trim_start().starts_with('#') {
            comments.get_or_insert(start);
            continue;
        }
        let above = comments.take().unwrap_or(start);
        let def = line
            .strip_prefix("async def ")
            .or_else(|| line.strip_prefix("def "));
        let top = def.is_some()
            || ["class ", "import ", "from "]
                .iter()
                .any(|prefix| line.starts_with(prefix));
        match items.last_mut() {
            Some(item) if !top => {
                item.span.end = start + line.len();
                item.marked |= line.trim_end() == marker;
            }
            _ => items.push(Item {
                span: above..start + line.len(),
                def: def.map(|rest| {
                    rest.split(|c: char| !unicode_ident::is_xid_continue(c))
                        .next()
                        .unwrap_or_default()
                }),
                marked: false,
            }),
        }
    }
    let empty = || Marked {
        document: base.clone(),
        captions: Vec::new(),
        spans: Vec::new(),
        main: false,
        unmarked: 0,
    };
    if !items.iter().any(|item| item.marked) {
        return Ok(empty());
    }
    let mut kernel = source.as_bytes().to_vec();
    let mut marked = Marked {
        spans: Vec::new(),
        main: false,
        unmarked: 0,
        ..empty()
    };
    for item in &items {
        let blank = match item.def {
            Some(name) if item.marked => {
                marked.main |= name == "main";
                marked.spans.push(item.span.clone());
                false
            }
            Some(_) => {
                marked.unmarked += 1;
                true
            }
            // A class serves both; an import, only authored source.
            None => !source[item.span.clone()].starts_with("class "),
        };
        if blank {
            for byte in &mut kernel[item.span.clone()] {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
        }
    }
    let kernel = String::from_utf8(kernel).map_err(|_| invalid("a region splits a character"))?;
    let (document, captions) = read_document(&kernel, base, limits, true)?;
    Ok(Marked {
        document,
        captions,
        ..marked
    })
}

fn read_document(
    source: &str,
    base: &k::Document,
    limits: GeneratedSourceLimits,
    marked: bool,
) -> Result<(k::Document, Vec<k::CaptionEntry>)> {
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
                let mut reader = Reader::new(source, base, limits)?;
                reader.marked = marked;
                let (mut document, captions) = reader.document(base)?;
                // Source spells no start adapter: the base keeps them.
                k::carry_start_adapters(base, &mut document);
                check_document_depth(&document, limits).map_err(|e| match e {
                    PrintError::Bytes { .. } => error(Code::GeneratedSourceBytes, e.to_string()),
                    PrintError::Depth { .. } => depth_error(),
                    _ => invalid(e.to_string()),
                })?;
                let captions = captions.resolve(&document);
                Ok((document, captions))
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
    fn metadata<T: serde::de::DeserializeOwned>(&mut self) -> Result<T> {
        self.text = self.text.trim_start_matches(' ');
        let mut depth = 0usize;
        let mut quoted = false;
        let mut escaped = false;
        let mut end = self.text.len();
        for (i, byte) in self.text.bytes().enumerate() {
            if quoted {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    quoted = false;
                }
            } else {
                match byte {
                    b'"' => quoted = true,
                    b'{' | b'[' => {
                        depth += 1;
                        if depth > k::KernelDataLimits::standard().document_json_depth() {
                            return Err(depth_error());
                        }
                    }
                    b'}' | b']' => {
                        depth = depth
                            .checked_sub(1)
                            .ok_or_else(|| invalid("unbalanced metadata"))?
                    }
                    b',' | b')' if depth == 0 => {
                        end = i;
                        break;
                    }
                    _ => {}
                }
            }
        }
        let native = &self.text[..end];
        let json =
            lash_kernel_dialect::metadata_json(native, lash_kernel_dialect::SourceFormat::PythonV1);
        let value = decode(&json)?;
        self.text = &self.text[end..];
        Ok(value)
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
    /// The base document's effects: a call of one's dotted path is a
    /// perform.
    effects: std::collections::BTreeSet<String>,
    scope: BindingScope,
    main: bool,
    operators: crate::generated_operators::Operators,
    /// The caption above each line that has one, by the line's index.
    captions: BTreeMap<usize, k::Caption>,
    found: captioned::Found,
    /// Whether each function opens with the kernel marker.
    marked: bool,
}
impl<'a> Reader<'a> {
    fn new(source: &'a str, base: &k::Document, limits: GeneratedSourceLimits) -> Result<Self> {
        if k::KernelVersion::of(base.manifest.kernel).is_none() {
            return Err(format_error("unsupported kernel version"));
        }
        let plan = lash_kernel_dialect::NamePlan::new(base, crate::printer::legal_identifier);
        let limit = limits.max_depth.min(64);
        let mut lines = vec![];
        let mut spelled = crate::captions::spelled(source, captioned::comments(source));
        let mut captions = BTreeMap::new();
        for (start, line) in captioned::lines(source) {
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
            if let Some(caption) = spelled.captions.remove(&(start + indent)) {
                captions.insert(lines.len(), caption);
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
            names: plan
                .names
                .into_iter()
                .filter(|(name, alias)| name.as_str() != alias)
                .map(|(name, alias)| (alias, name))
                .collect(),
            helpers: plan
                .helpers
                .into_iter()
                .map(|(id, alias)| (alias, id))
                .collect(),
            effects: base
                .manifest
                .effects
                .keys()
                .map(|effect| effect.as_str().to_owned())
                .collect(),
            scope: Default::default(),
            main: false,
            operators: crate::generated_operators::Operators::new(&base.manifest.functions),
            captions,
            found: Default::default(),
            marked: false,
        })
    }
    /// The kernel marker opening a function's body.
    fn marker(&mut self) -> Result<()> {
        let marker = format!("\"{}\"", crate::printer::KERNEL_MARKER);
        match self.lines.get(self.next) {
            Some(line) if line.indent == 1 && line.text == marker => {
                self.next += 1;
                Ok(())
            }
            _ => Err(format_error(
                "a kernel-form region opens with the kernel marker",
            )),
        }
    }
    /// The caption above the line read next.
    fn caption(&self) -> Option<k::Caption> {
        self.captions.get(&self.next).cloned()
    }
    /// Begins the statement on the line read next, which the caption above
    /// it names, and answers its ordinal.
    fn begin_statement(&mut self) -> usize {
        let caption = self.caption();
        self.found.statement(caption)
    }
    /// Notes the caption above the line read next for block `child` of the
    /// statement `owner`: the clause that line begins.
    fn arm(&mut self, owner: usize, child: u32) {
        let caption = self.caption();
        self.found.arm(owner, child, caption);
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
        let native;
        let name = if let Some(name) = self.names.get(alias) {
            name
        } else {
            if !crate::printer::legal_identifier(alias)
                || self.names.values().any(|name| name.as_str() == alias)
            {
                return Err(invalid("undeclared identifier alias"));
            }
            native = k::Name::new(alias);
            &native
        };
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
    /// A `TypedDict` above the functions, after its `class Name`: its
    /// fields as written and the caption above each. The source spells
    /// types only for their fields' captions; the document's are the
    /// base's.
    fn class(&mut self, c: &mut Cursor<'_>) -> Result<Vec<captioned::ClassField>> {
        let header = c.text.trim_end();
        if !(header.starts_with("(TypedDict") && header.ends_with("):")) {
            return Err(format_error("a class is a TypedDict"));
        }
        let mut fields = Vec::new();
        while self.more(1) {
            let caption = self.caption();
            let (mut line, _) = self.line(1)?;
            if line.take("pass") {
                line.finish()?;
                continue;
            }
            let name = line.word()?.to_owned();
            line.expect(":")?;
            fields.push(captioned::ClassField {
                name,
                annotation: line.text.trim().to_owned(),
                caption,
            });
        }
        Ok(fields)
    }
    /// A `def`'s parameters and the `:` that ends its header: on its line,
    /// or one a line, each below the caption above it and with the
    /// annotation it is written with, then `)`, the result's annotation
    /// and `:` on a line of their own.
    fn signature(
        &mut self,
        c: &mut Cursor<'_>,
    ) -> Result<(Vec<k::Name>, Vec<captioned::WrittenParam>, Option<String>)> {
        if c.text.trim() != "(" {
            let params = self.parameters(c)?;
            c.expect(":")?;
            c.finish()?;
            return Ok((params, Vec::new(), None));
        }
        let mut params = Vec::new();
        let mut written = Vec::new();
        while self.more(1) {
            let caption = self.caption();
            let (mut line, _) = self.line(1)?;
            params.push(self.name(&mut line)?);
            let rest = line.text.trim();
            let rest = rest
                .strip_suffix(',')
                .ok_or_else(|| invalid("expected , after a parameter"))?;
            let annotation = match rest.strip_prefix(':') {
                Some(annotation) => Some(annotation.trim().to_owned()),
                None if rest.trim().is_empty() => None,
                None => return Err(invalid("expected : or , after a parameter")),
            };
            written.push(captioned::WrittenParam {
                caption,
                annotation,
            });
        }
        let (mut close, _) = self.line(0)?;
        close.expect(")")?;
        let rest = close.text.trim();
        let rest = rest
            .strip_suffix(':')
            .ok_or_else(|| invalid("expected : after a signature"))?;
        let result = match rest.trim().strip_prefix("->") {
            Some(annotation) => Some(annotation.trim().to_owned()),
            None if rest.trim().is_empty() => None,
            None => return Err(invalid("expected -> or : after a signature")),
        };
        Ok((params, written, result))
    }
    fn parameters(&self, c: &mut Cursor<'_>) -> Result<Vec<k::Name>> {
        c.expect("(")?;
        let mut params = vec![];
        if !c.take(")") {
            loop {
                params.push(self.name(c)?);
                if c.take(")") {
                    break;
                }
                c.expect(",")?;
            }
        }
        Ok(params)
    }
    /// A whole right-hand side. A library action whose operands are all
    /// atoms is the statement action, a leaf; any other stays an expression.
    fn inline_rhs(&self, c: &mut Cursor<'_>, depth: usize) -> Result<k::Rhs> {
        self.depth(depth)?;
        if c.text.trim_start().starts_with("k.") && Self::action_form(c.text) {
            return self.action_cursor(c).map(k::Rhs::Action);
        }
        let (expr, height) = self.inline_value(c, 0)?;
        if let k::Expr::Act(act) = &expr
            && let Some(action) = act.as_action()
        {
            return Ok(k::Rhs::Action(action));
        }
        self.depth(depth + height - 1)?;
        Ok(k::Rhs::Expr(expr))
    }
    fn await_call(&self, c: &mut Cursor<'_>) -> Result<k::Action> {
        self.awaited_expr(c, 0)?
            .0
            .as_action()
            .ok_or_else(|| invalid("a statement-position action requires atom arguments"))
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
    fn document(mut self, base: &k::Document) -> Result<(k::Document, captioned::Found)> {
        let mut document = k::Document {
            manifest: base.manifest.clone(),
            entries: base.entries.clone(),
            private_bindings: base.private_bindings.clone(),
            functions: BTreeMap::new(),
            main: vec![],
        };
        let mut main = false;
        let mut classes = captioned::Classes::new();
        while self.more(0) {
            let caption = self.caption();
            let (mut c, suite) = self.line(0)?;
            if !suite && document.functions.is_empty() && !main && c.take("class ") {
                let name = c.word()?.to_owned();
                let fields = self.class(&mut c)?;
                if classes.insert(name, fields).is_some() {
                    return Err(format_error("duplicate class"));
                }
                continue;
            }
            if !suite && (c.take("async def ") || c.take("def ")) && !main {
                let alias = c.word()?;
                let name = if alias == "main" {
                    None
                } else {
                    Some(self.named(alias)?)
                };
                let unit = name.clone().map_or(k::Unit::Main, k::Unit::Function);
                self.found.root(unit, caption);
                let (params, written, result) = self.signature(&mut c)?;
                if let Some(name) = &name {
                    self.found
                        .signature(name, &params, written, result.as_deref(), &classes);
                } else if !written.is_empty() || result.is_some() {
                    return Err(format_error("main takes no annotated parameters"));
                }
                self.main = name.is_none();
                self.scope = BindingScope::new(&params);
                if self.marked {
                    self.marker()?;
                }
                let body = self.readable_block(1, 0)?;
                if let Some(name) = name {
                    if document
                        .functions
                        .insert(
                            name,
                            k::Function {
                                params,
                                body,
                                start: None,
                            },
                        )
                        .is_some()
                    {
                        return Err(format_error("duplicate declaration"));
                    }
                } else {
                    if !params.is_empty() {
                        return Err(format_error("main takes no parameters"));
                    }
                    document.main = body;
                    main = true;
                }
                continue;
            }
            return Err(format_error(
                "expected function definitions followed by exactly one main",
            ));
        }
        if !main || self.next != self.lines.len() {
            return Err(format_error("missing main or trailing program text"));
        }
        Ok((document, self.found))
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
        let ordinal = self.begin_statement();
        let (mut c, suite) = self.line(indent)?;
        if !suite {
            let stmt = self.readable_statement(c, indent, depth, ordinal)?;
            self.scope
                .types
                .statement(&stmt, self.operators.functions());
            return Ok(stmt);
        }
        let op = c.operation()?;
        let i = indent + 1;
        let d = depth + 1;
        let stmt = match op {
            "let" | "assign" => {
                let name = self.name(&mut c)?;
                self.scope.bind(&name, Some(op == "let"));
                c.end(true)?;
                let value = self.rhs(i, d)?;
                if op == "let" {
                    k::Stmt::Let { name, value }
                } else {
                    k::Stmt::Assign {
                        place: k::Place::Variable(name),
                        value,
                        order: k::AssignOrder::ValueFirst,
                    }
                }
            }
            "assign_field" | "assign_index" | "remove_field" | "remove_index" => {
                let member = self.member(op.ends_with("field"), &mut c, i, d)?;
                if op.starts_with("assign_") {
                    k::Stmt::Assign {
                        place: k::Place::Member(member),
                        value: self.rhs(i, d)?,
                        order: k::AssignOrder::ValueFirst,
                    }
                } else {
                    k::Stmt::Remove { member }
                }
            }
            "do" => {
                c.end(true)?;
                k::Stmt::Do {
                    value: self.expr(i, d)?,
                }
            }
            "if_" => {
                c.end(true)?;
                let condition = self.expr(i, d)?;
                let then_block = self.block(i, d)?;
                self.arm(ordinal, 2);
                k::Stmt::If {
                    condition,
                    then_block,
                    else_block: self.block(i, d)?,
                }
            }
            "for_" => {
                let binding = self.name(&mut c)?;
                self.scope.introduce(&binding);
                let via = if c.take(",") {
                    let open = self.id(&mut c)?;
                    let hook = |c: &mut Cursor<'_>| -> Result<Option<k::FunctionId>> {
                        c.expect(",")?;
                        if c.take("None") {
                            Ok(None)
                        } else {
                            self.id(c).map(Some)
                        }
                    };
                    Some(k::Protocol {
                        open,
                        next: hook(&mut c)?,
                        item: hook(&mut c)?,
                        check: hook(&mut c)?,
                    })
                } else {
                    None
                };
                c.end(true)?;
                k::Stmt::For {
                    metered: 0,
                    binding,
                    iterable: self.expr(i, d)?,
                    via,
                    body: self.block(i, d)?,
                    else_block: self.generic_else(i, d, ordinal)?,
                }
            }
            "while_" => {
                c.end(true)?;
                k::Stmt::While {
                    metered: 0,
                    condition: self.expr(i, d)?,
                    body: self.block(i, d)?,
                    else_block: self.generic_else(i, d, ordinal)?,
                }
            }
            "return_via" => {
                let policy = self.id(&mut c)?;
                c.end(true)?;
                k::Stmt::ReturnVia {
                    value: self.expr(i, d)?,
                    policy,
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
                    let name = self.named(alias)?;
                    self.scope.introduce(&name);
                    Some(name)
                };
                c.expect(",")?;
                let finally = match c.word()? {
                    "True" => true,
                    "False" => false,
                    _ => return Err(invalid("expected finally presence flag")),
                };
                c.end(true)?;
                let body = self.block(i, d)?;
                if binding.is_some() {
                    self.arm(ordinal, 1);
                }
                let catch = binding
                    .map(|binding| self.block(i, d).map(|body| k::Catch { binding, body }))
                    .transpose()?;
                let finally = if finally {
                    self.arm(ordinal, 1 + u32::from(catch.is_some()));
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
        self.scope
            .types
            .statement(&stmt, self.operators.functions());
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
    /// A loop's `else` suite, or the empty block when it has none.
    fn readable_else(&mut self, indent: usize, depth: usize, owner: usize) -> Result<k::Block> {
        if !self.starts(indent, "else:") {
            return Ok(Vec::new());
        }
        self.arm(owner, 2);
        let (mut c, _) = self.line(indent)?;
        c.expect("else:")?;
        c.finish()?;
        let block = self.readable_block(indent + 1, depth)?;
        if block.is_empty() {
            return Err(invalid("an empty loop else is omitted"));
        }
        Ok(block)
    }
    /// A generic loop's `else` block, when one follows its body.
    fn generic_else(&mut self, indent: usize, depth: usize, owner: usize) -> Result<k::Block> {
        if !self.more(indent) {
            return Ok(Vec::new());
        }
        self.arm(owner, 2);
        let block = self.block(indent, depth)?;
        if block.is_empty() {
            return Err(invalid("an empty loop else is omitted"));
        }
        Ok(block)
    }
    fn readable_if(
        &mut self,
        mut c: Cursor<'_>,
        indent: usize,
        depth: usize,
        ordinal: usize,
    ) -> Result<k::Stmt> {
        self.depth(depth)?;
        let condition = self.inline(&mut c, depth + 1)?;
        c.expect(":")?;
        c.finish()?;
        let then_block = self.readable_block(indent + 1, depth + 1)?;
        let else_block = if self.starts(indent, "elif ") {
            // Exact inverse: elif is one If statement inside the else Block.
            self.depth(depth + 1)?;
            let nested = self.begin_statement();
            let (mut c, _) = self.line(indent)?;
            c.expect("elif ")?;
            vec![self.readable_if(c, indent, depth + 2, nested)?]
        } else {
            self.arm(ordinal, 2);
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
    fn action_form(text: &str) -> bool {
        if text.trim_start().starts_with("await ") {
            return true;
        }
        let mut probe = Cursor::new(text);
        probe
            .operation()
            .is_ok_and(|op| matches!(op, "call" | "perform" | "cancel" | "yield_"))
    }
    fn readable_statement(
        &mut self,
        mut c: Cursor<'_>,
        indent: usize,
        depth: usize,
        ordinal: usize,
    ) -> Result<k::Stmt> {
        let field_operation = if c.take("setattr(") {
            Some(true)
        } else if c.take("delattr(") {
            Some(false)
        } else {
            None
        };
        let stmt = if let Some(assign) = field_operation {
            let target = self.inline(&mut c, depth + 1)?;
            c.expect(",")?;
            let field = c.string()?;
            let member = k::Member::Field { target, field };
            let stmt = if assign {
                c.expect(",")?;
                let value = self.inline_rhs(&mut c, depth + 1)?;
                k::Stmt::Assign {
                    place: k::Place::Member(member),
                    value,
                    order: k::AssignOrder::ValueFirst,
                }
            } else {
                k::Stmt::Remove { member }
            };
            c.expect(")")?;
            c.finish()?;
            stmt
        } else if c.take("async def ") || c.take("def ") {
            let name = self.name(&mut c)?;
            self.depth(depth + 1)?;
            let params = self.parameters(&mut c)?;
            let explicit = if c.take("-> Final") {
                Some(true)
            } else if c.take("-> reassigned") {
                Some(false)
            } else {
                None
            };
            let (declaration, _) = self.scope.bind(&name, explicit);
            c.expect(":")?;
            c.finish()?;
            let parent = std::mem::replace(&mut self.scope, BindingScope::new(&params));
            let outer_main = self.main;
            self.main = false;
            let body = self.readable_block(indent + 1, depth + 2)?;
            self.main = outer_main;
            self.scope = parent;
            let value = k::Rhs::Expr(k::Expr::Closure(Box::new(k::Closure {
                params,
                body,
                start: None,
            })));
            if declaration {
                k::Stmt::Let { name, value }
            } else {
                k::Stmt::Assign {
                    place: k::Place::Variable(name),
                    value,
                    order: k::AssignOrder::ValueFirst,
                }
            }
        } else if c.take("del ") {
            // The target's Member has no Site. Its children start at depth+1.
            let target = self.inline(&mut c, depth)?;
            c.finish()?;
            let k::Expr::Member(member) = target else {
                return Err(invalid("del requires a member"));
            };
            k::Stmt::Remove { member: *member }
        } else if c.take("await ") {
            self.depth(depth + 1)?;
            let (act, _) = self.awaited_expr(&mut c, depth + 1)?;
            c.finish()?;
            k::Stmt::Do {
                value: k::Expr::Act(Box::new(act)),
            }
        } else if c.take("if ") {
            self.readable_if(c, indent, depth, ordinal)?
        } else if c.take("for ") {
            let binding = self.name(&mut c)?;
            self.scope.introduce(&binding);
            c.expect("in ")?;
            // No inline expression ends in a call, so a view's call ends
            // the line only where the loop walks the view.
            let header = c.text.trim_end();
            let (walked, view) = ["keys", "values", "items"]
                .into_iter()
                .find_map(|view| {
                    header
                        .strip_suffix(&format!(".{view}():"))
                        .map(|walked| (walked, Some(view)))
                })
                .unwrap_or((header.strip_suffix(':').unwrap_or(header), None));
            let mut walked = Cursor::new(walked);
            let iterable = self.inline(&mut walked, depth + 1)?;
            walked.finish()?;
            // The base's `py.for_in` protocol, or the kernel's own loop in
            // a base that labels none.
            let via = match (self.operators.for_in(None), view) {
                (None, None) => None,
                (Some(_), view) => Some(
                    self.operators
                        .for_in(view)
                        .ok_or_else(|| invalid("the base document holds no view protocol"))?,
                ),
                (None, Some(_)) => return Err(invalid("a view loop needs py.for_in")),
            };
            if !header.ends_with(':') {
                return Err(invalid("expected :"));
            }
            k::Stmt::For {
                metered: 0,
                binding,
                iterable,
                via,
                body: self.readable_block(indent + 1, depth + 1)?,
                else_block: self.readable_else(indent, depth + 1, ordinal)?,
            }
        } else if c.take("while ") {
            let condition = self.inline(&mut c, depth + 1)?;
            c.expect(":")?;
            c.finish()?;
            k::Stmt::While {
                metered: 0,
                condition,
                body: self.readable_block(indent + 1, depth + 1)?,
                else_block: self.readable_else(indent, depth + 1, ordinal)?,
            }
        } else if c.take("try:") {
            c.finish()?;
            let body = self.readable_block(indent + 1, depth + 1)?;
            let catch = if self.starts(indent, "except ") {
                self.arm(ordinal, 1);
                let (mut c, suite) = self.line(indent)?;
                if suite {
                    return Err(invalid("expected except clause"));
                }
                c.expect("except BaseException as ")?;
                let binding = self.name(&mut c)?;
                self.scope.introduce(&binding);
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
                self.arm(ordinal, 1 + u32::from(catch.is_some()));
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
        } else if c.take("return returned(") {
            let value = self.inline(&mut c, depth + 1)?;
            c.expect(")")?;
            c.finish()?;
            k::Stmt::Return { value }
        } else if c.take("return finish(") {
            let value = self.inline(&mut c, depth + 1)?;
            c.expect(")")?;
            c.finish()?;
            k::Stmt::Finish { value }
        } else if c.take("raise failure(") {
            let value = self.inline(&mut c, depth + 1)?;
            c.expect(")")?;
            c.finish()?;
            k::Stmt::Fail { value }
        } else if c.take("print(") {
            let value = self.inline(&mut c, depth + 1)?;
            c.expect(")")?;
            c.finish()?;
            k::Stmt::Print { value }
        } else if c.take("return ") {
            let value = self.inline(&mut c, depth + 1)?;
            c.finish()?;
            if self.main {
                k::Stmt::Finish { value }
            } else {
                k::Stmt::Return { value }
            }
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
        } else if c.text.starts_with("spawn(") || c.text.starts_with("await_by(") {
            let k::Expr::Act(act) = self.inline(&mut c, depth + 1)? else {
                return Err(invalid("expected a task action"));
            };
            c.finish()?;
            k::Stmt::Do {
                value: k::Expr::Act(act),
            }
        } else if !c.text.starts_with("k.") {
            // Parse the place separately from the RHS; an annotation is the
            // exact stored distinction between declaration and reassignment.
            let target = self.inline(&mut c, depth)?;
            if c.text.trim().is_empty() {
                k::Stmt::Do { value: target }
            } else {
                let explicit = if c.take(": Final") {
                    Some(true)
                } else if c.take(": reassigned") {
                    Some(false)
                } else {
                    None
                };
                let declaration = if let k::Expr::Variable(name) = &target {
                    self.scope.bind(name, explicit).0
                } else {
                    explicit.unwrap_or(false)
                };
                c.expect("=")?;
                let value = self.inline_rhs(&mut c, depth + 1)?;
                c.finish()?;
                match target {
                    k::Expr::Variable(name) if declaration => k::Stmt::Let { name, value },
                    k::Expr::Variable(name) => k::Stmt::Assign {
                        place: k::Place::Variable(name),
                        value,
                        order: k::AssignOrder::ValueFirst,
                    },
                    k::Expr::Member(m) if !declaration => k::Stmt::Assign {
                        place: k::Place::Member(*m),
                        value,
                        order: k::AssignOrder::ValueFirst,
                    },
                    _ => return Err(invalid("invalid assignment place")),
                }
            }
        } else if Self::action_form(c.text) {
            self.depth(depth + 1)?;
            let action = self.action_cursor(&mut c)?;
            c.finish()?;
            k::Stmt::Do {
                value: action.into(),
            }
        } else {
            let mut probe = Cursor::new(c.text);
            let op = probe.operation()?;
            let stmt = match op {
                "break_" => k::Stmt::Break,
                "continue_" => k::Stmt::Continue,
                "print" | "finish" | "fail" => {
                    let value = self.inline(&mut probe, depth + 1)?;
                    match op {
                        "print" => k::Stmt::Print { value },
                        "finish" => k::Stmt::Finish { value },
                        _ => k::Stmt::Fail { value },
                    }
                }
                _ => {
                    let value = self.inline(&mut c, depth + 1)?;
                    c.finish()?;
                    self.closed(indent)?;
                    return Ok(k::Stmt::Do { value });
                }
            };
            probe.end(false)?;
            stmt
        };
        self.closed(indent)?;
        Ok(stmt)
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
        let action = Self::action_form(line.text);
        if action {
            self.action_line(indent, depth).map(k::Rhs::Action)
        } else {
            self.expr(indent, depth).map(|expr| match expr {
                k::Expr::Act(act) => match act.as_action() {
                    Some(action) => k::Rhs::Action(action),
                    None => k::Rhs::Expr(k::Expr::Act(act)),
                },
                expr => k::Rhs::Expr(expr),
            })
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
            "call" | "spawn" | "perform" | "sleep" | "join" | "join_many" | "await_" | "yield_"
            | "cancel" => self.nested_action_suite(op, &mut c, i, d)?,
            "field" | "index" => {
                k::Expr::Member(Box::new(self.member(op == "field", &mut c, i, d)?))
            }
            "closure" => {
                let params = self.names(&mut c)?;
                c.end(true)?;
                let parent = std::mem::replace(&mut self.scope, BindingScope::new(&params));
                let outer_main = self.main;
                self.main = false;
                let body = self.block(i, d)?;
                self.main = outer_main;
                self.scope = parent;
                k::Expr::Closure(Box::new(k::Closure {
                    params,
                    body,
                    start: None,
                }))
            }
            "choose" => {
                c.end(true)?;
                k::Expr::If {
                    cond: Box::new(self.expr(i, d)?),
                    then: Box::new(self.expr(i, d)?),
                    r#else: Box::new(self.expr(i, d)?),
                }
            }
            "bind" => {
                let name = k::Name::new(c.string()?);
                c.end(true)?;
                k::Expr::Let {
                    name,
                    value: Box::new(self.expr(i, d)?),
                    body: Box::new(self.expr(i, d)?),
                }
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
        let action = if c.take("await ") {
            self.await_call(&mut c)?
        } else {
            self.action_cursor(&mut c)?
        };
        c.finish()?;
        Ok(action)
    }
    fn action_cursor(&self, c: &mut Cursor<'_>) -> Result<k::Action> {
        let action = match c.operation()? {
            "call" => {
                let (callee, args) = self.action_operands(c)?;
                k::Action::Call { callee, args }
            }
            "perform" => {
                let effect = k::EffectName::new(c.string()?).map_err(|e| invalid(e.to_string()))?;
                c.expect(",")?;
                let args = self.atoms(c)?;
                c.expect(",")?;
                if c.operation()? != "type" {
                    return Err(invalid("expected complete result type"));
                }
                let result = c.metadata()?;
                c.expect(")")?;
                k::Action::Perform {
                    effect,
                    args,
                    result,
                    then_yield: false,
                }
            }
            "cancel" => k::Action::Cancel {
                task: self.atom(c)?,
            },
            "yield_" => k::Action::Yield,
            _ => return Err(invalid("unknown action form")),
        };
        c.expect(")")?;
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
    check_metadata_uniqueness(text).map_err(|e| format_error(e.to_string()))?;
    let mut decoder = serde_json::Deserializer::from_str(text);
    decoder.disable_recursion_limit();
    let value = T::deserialize(&mut decoder).map_err(|e| format_error(e.to_string()))?;
    decoder.end().map_err(|e| format_error(e.to_string()))?;
    Ok(value)
}
