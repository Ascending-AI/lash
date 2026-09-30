//! The source analysis behind the replay-read gate: a Rust tokenizer that
//! keeps delimiters as nested groups, and the walk that finds replay-path
//! functions, their recorded-step spans and the store reads outside them.
//!
//! It is deliberately not a Rust parser. It needs only what a delimiter tree
//! gives exactly: whether a call's argument list contains a token, which body
//! a `fn` item owns, and which items a test attribute removes.

use std::collections::{BTreeMap, BTreeSet};

/// One token, with the 1-based line it starts on.
#[derive(Debug, Clone)]
pub(super) struct Token {
    pub(super) kind: Kind,
    pub(super) line: usize,
}

#[derive(Debug, Clone)]
pub(super) enum Kind {
    Ident(String),
    Punct(char),
    /// A string, char or number literal, with its source text.
    Literal(String),
    Lifetime,
    Group(Delimiter, Vec<Token>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Delimiter {
    Paren,
    Bracket,
    Brace,
}

impl Token {
    fn ident(&self) -> Option<&str> {
        match &self.kind {
            Kind::Ident(name) => Some(name),
            _ => None,
        }
    }

    fn is_punct(&self, c: char) -> bool {
        matches!(self.kind, Kind::Punct(p) if p == c)
    }

    fn group(&self, delimiter: Delimiter) -> Option<&[Token]> {
        match &self.kind {
            Kind::Group(d, tokens) if *d == delimiter => Some(tokens),
            _ => None,
        }
    }
}

/// Tokenize `source` into a delimiter tree. An unbalanced delimiter is an
/// error: the gate must never silently read a file it cannot parse.
pub(super) fn tokenize(source: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = source.chars().collect();
    let mut stack: Vec<(Delimiter, usize, Vec<Token>)> = Vec::new();
    let mut current: Vec<Token> = Vec::new();
    let mut i = 0;
    let mut line = 1;
    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            let mut depth = 0usize;
            while i < chars.len() {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    if chars[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
            continue;
        }
        let start_line = line;
        if c == '"' {
            let start = i;
            i = skip_quoted(&chars, i, '"', &mut line)?;
            current.push(Token {
                kind: Kind::Literal(chars[start..i].iter().collect()),
                line: start_line,
            });
            continue;
        }
        if c == '\'' {
            // A char literal is `'x'` or an escape; anything else is a
            // lifetime or a loop label.
            if chars.get(i + 1) == Some(&'\\') || chars.get(i + 2) == Some(&'\'') {
                let start = i;
                i = skip_quoted(&chars, i, '\'', &mut line)?;
                current.push(Token {
                    kind: Kind::Literal(chars[start..i].iter().collect()),
                    line: start_line,
                });
            } else {
                i += 1;
                while i < chars.len() && is_ident_continue(chars[i]) {
                    i += 1;
                }
                current.push(Token {
                    kind: Kind::Lifetime,
                    line: start_line,
                });
            }
            continue;
        }
        if is_ident_start(c) {
            let start = i;
            while i < chars.len() && is_ident_continue(chars[i]) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            // Raw and byte string prefixes, and raw identifiers.
            if matches!(word.as_str(), "r" | "br" | "cr")
                && matches!(chars.get(i), Some('"') | Some('#'))
            {
                let mut hashes = 0;
                let mut j = i;
                while chars.get(j) == Some(&'#') {
                    hashes += 1;
                    j += 1;
                }
                if chars.get(j) == Some(&'"') {
                    i = skip_raw(&chars, j, hashes, &mut line)?;
                    current.push(Token {
                        kind: Kind::Literal(chars[start..i].iter().collect()),
                        line: start_line,
                    });
                    continue;
                }
                if word == "r" && hashes == 1 && chars.get(j).is_some_and(|c| is_ident_start(*c)) {
                    let ident_start = j;
                    i = j;
                    while i < chars.len() && is_ident_continue(chars[i]) {
                        i += 1;
                    }
                    current.push(Token {
                        kind: Kind::Ident(chars[ident_start..i].iter().collect()),
                        line: start_line,
                    });
                    continue;
                }
            }
            if matches!(word.as_str(), "b" | "c") && chars.get(i) == Some(&'"') {
                i = skip_quoted(&chars, i, '"', &mut line)?;
                current.push(Token {
                    kind: Kind::Literal(chars[start..i].iter().collect()),
                    line: start_line,
                });
                continue;
            }
            if word == "b" && chars.get(i) == Some(&'\'') {
                i = skip_quoted(&chars, i, '\'', &mut line)?;
                current.push(Token {
                    kind: Kind::Literal(chars[start..i].iter().collect()),
                    line: start_line,
                });
                continue;
            }
            current.push(Token {
                kind: Kind::Ident(word),
                line: start_line,
            });
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len()
                && (is_ident_continue(chars[i])
                    || (chars[i] == '.'
                        && chars.get(i + 1).is_some_and(|next| next.is_ascii_digit())))
            {
                i += 1;
            }
            current.push(Token {
                kind: Kind::Literal(chars[start..i].iter().collect()),
                line: start_line,
            });
            continue;
        }
        let open = match c {
            '(' => Some(Delimiter::Paren),
            '[' => Some(Delimiter::Bracket),
            '{' => Some(Delimiter::Brace),
            _ => None,
        };
        if let Some(delimiter) = open {
            stack.push((delimiter, start_line, std::mem::take(&mut current)));
            i += 1;
            continue;
        }
        let close = match c {
            ')' => Some(Delimiter::Paren),
            ']' => Some(Delimiter::Bracket),
            '}' => Some(Delimiter::Brace),
            _ => None,
        };
        if let Some(delimiter) = close {
            let Some((open, open_line, outer)) = stack.pop() else {
                return Err(format!("line {line}: unmatched `{c}`"));
            };
            if open != delimiter {
                return Err(format!(
                    "line {line}: `{c}` closes a group opened on {open_line}"
                ));
            }
            let inner = std::mem::replace(&mut current, outer);
            current.push(Token {
                kind: Kind::Group(delimiter, inner),
                line: open_line,
            });
            i += 1;
            continue;
        }
        current.push(Token {
            kind: Kind::Punct(c),
            line: start_line,
        });
        i += 1;
    }
    if let Some((_, open_line, _)) = stack.last() {
        return Err(format!("group opened on line {open_line} never closes"));
    }
    Ok(current)
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

/// The index just past a quoted literal starting at `start`.
fn skip_quoted(
    chars: &[char],
    start: usize,
    quote: char,
    line: &mut usize,
) -> Result<usize, String> {
    let mut i = start + 1;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                if chars.get(i + 1) == Some(&'\n') {
                    *line += 1;
                }
                i += 2;
            }
            '\n' => {
                *line += 1;
                i += 1;
            }
            c if c == quote => return Ok(i + 1),
            _ => i += 1,
        }
    }
    Err(format!("line {line}: unterminated literal"))
}

/// The index just past a raw string whose opening quote is at `quote`.
fn skip_raw(
    chars: &[char],
    quote: usize,
    hashes: usize,
    line: &mut usize,
) -> Result<usize, String> {
    let mut i = quote + 1;
    while i < chars.len() {
        if chars[i] == '\n' {
            *line += 1;
        }
        if chars[i] == '"' && (0..hashes).all(|k| chars.get(i + 1 + k) == Some(&'#')) {
            return Ok(i + 1 + hashes);
        }
        i += 1;
    }
    Err(format!("line {line}: unterminated raw string"))
}

/// What the analysis is told about the API surface and the replay paths.
pub(super) struct Surface<'a> {
    /// Store read methods: a call to one is a fresh read of mutable state.
    pub(super) reads: &'a [&'a str],
    /// Calls whose argument list is a recorded step's body.
    pub(super) steps: &'a [&'a str],
    /// A `fn` whose signature names one of these is on a replay path.
    pub(super) replay_signature_types: &'a [&'a str],
    /// Every method of an `impl` of one of these types is on a replay path.
    pub(super) replay_impl_types: &'a [&'a str],
}

/// One `fn` item with a body.
#[derive(Debug)]
struct FnDef {
    name: String,
    file: usize,
    /// Why the fn is on a replay path by itself, if it is.
    seed: Option<String>,
    reads: Vec<ReadSite>,
    /// Names called outside every recorded step.
    unrecorded_calls: BTreeSet<String>,
}

#[derive(Debug)]
struct ReadSite {
    method: String,
    line: usize,
    recorded: bool,
}

/// A fresh read on a replay path, outside every recorded step.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Hit {
    pub(super) file: String,
    pub(super) line: usize,
    /// The hit's source line, trimmed, whitespace runs collapsed: the key a
    /// pin names, so moving the line keeps its pin.
    pub(super) text: String,
    pub(super) method: String,
    pub(super) function: String,
    /// How the function came to be on a replay path.
    pub(super) path: String,
}

/// Analyze `files` (`(path, source)`): every surface read inside a
/// replay-path function and outside every recorded-step span.
pub(super) fn analyze(
    files: &[(String, String)],
    surface: &Surface<'_>,
) -> Result<Vec<Hit>, String> {
    let mut trees = Vec::new();
    for (path, source) in files {
        trees.push(tokenize(source).map_err(|error| format!("{path}: {error}"))?);
    }
    let owning = replay_owning_types(&trees, surface);
    let mut fns = Vec::new();
    for (index, tokens) in trees.iter().enumerate() {
        let restate = files[index].0.contains("lash-restate/");
        let context = Context {
            surface,
            owning: &owning,
            restate,
        };
        collect_items(tokens, index, &[], &context, &mut fns);
    }
    let mut by_name: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, def) in fns.iter().enumerate() {
        by_name.entry(def.name.as_str()).or_default().push(index);
    }
    // Replay-path functions: the seeds, then every uniquely named function a
    // replay-path body calls outside its recorded steps, to a fixpoint.
    let mut via: BTreeMap<usize, String> = BTreeMap::new();
    let mut pending = Vec::new();
    for (index, def) in fns.iter().enumerate() {
        if let Some(seed) = &def.seed {
            via.insert(index, seed.clone());
            pending.push(index);
        }
    }
    while let Some(index) = pending.pop() {
        for callee in &fns[index].unrecorded_calls {
            let Some(defs) = by_name.get(callee.as_str()) else {
                continue;
            };
            let [only] = defs.as_slice() else {
                continue;
            };
            if !via.contains_key(only) {
                via.insert(*only, format!("called from `{}`", fns[index].name));
                pending.push(*only);
            }
        }
    }
    let mut hits = Vec::new();
    for (index, path) in via {
        let def = &fns[index];
        let (file, source) = &files[def.file];
        for read in def.reads.iter().filter(|read| !read.recorded) {
            let text = source
                .lines()
                .nth(read.line - 1)
                .map(normalize)
                .unwrap_or_default();
            hits.push(Hit {
                file: file.clone(),
                line: read.line,
                text,
                method: read.method.clone(),
                function: def.name.clone(),
                path: path.clone(),
            });
        }
    }
    hits.sort();
    Ok(hits)
}

/// A line trimmed, with every whitespace run collapsed to one space.
pub(super) fn normalize(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether an attribute's tokens (the `[...]` group) gate test-only code:
/// `#[test]`, `#[tokio::test]`, or a `cfg` naming `test` or the `testing`
/// feature without negating it.
fn is_test_attribute(tokens: &[Token]) -> bool {
    let idents: Vec<&str> = flatten_idents(tokens);
    let literal_testing = contains_literal(tokens, "\"testing\"");
    match idents.first() {
        Some(&"test") => true,
        Some(&"tokio") => idents.get(1) == Some(&"test"),
        Some(&"cfg") => !idents.contains(&"not") && (idents.contains(&"test") || literal_testing),
        _ => false,
    }
}

fn contains_literal(tokens: &[Token], text: &str) -> bool {
    tokens.iter().any(|token| match &token.kind {
        Kind::Literal(literal) => literal == text,
        Kind::Group(_, inner) => contains_literal(inner, text),
        _ => false,
    })
}

fn flatten_idents(tokens: &[Token]) -> Vec<&str> {
    let mut out = Vec::new();
    for token in tokens {
        match &token.kind {
            Kind::Ident(name) => out.push(name.as_str()),
            Kind::Group(_, inner) => out.extend(flatten_idents(inner)),
            _ => {}
        }
    }
    out
}

/// What the item walk consults.
struct Context<'a> {
    surface: &'a Surface<'a>,
    /// Types that hold a controller or journal context: the explicit replay
    /// impl types, and every struct or alias whose definition names one.
    owning: &'a BTreeSet<String>,
    /// Whether the file is in `lash-restate`, where `Context` is Restate's.
    restate: bool,
}

/// Every type that holds a controller or journal context itself: a `type`
/// alias naming one (or another such alias), and a `struct` with a field
/// naming one, plus the explicit replay impl types. Holding a struct that
/// holds one does not count: a facade that owns a core is not on a replay
/// path for every method it has, only where it takes or keeps a controller.
fn replay_owning_types(trees: &[Vec<Token>], surface: &Surface<'_>) -> BTreeSet<String> {
    let mut definitions: Vec<TypeDefinition> = Vec::new();
    for tokens in trees {
        collect_type_definitions(tokens, &mut definitions);
    }
    let mut aliases: BTreeSet<String> = BTreeSet::new();
    loop {
        let before = aliases.len();
        for definition in definitions.iter().filter(|definition| definition.alias) {
            let names = definition.idents.iter().any(|ident| {
                surface.replay_signature_types.contains(&ident.as_str()) || aliases.contains(ident)
            });
            if names {
                aliases.insert(definition.name.clone());
            }
        }
        if aliases.len() == before {
            break;
        }
    }
    let mut owning: BTreeSet<String> = surface
        .replay_impl_types
        .iter()
        .map(|ty| (*ty).to_string())
        .collect();
    for definition in definitions.iter().filter(|definition| !definition.alias) {
        let holds = definition.idents.iter().any(|ident| {
            surface.replay_signature_types.contains(&ident.as_str()) || aliases.contains(ident)
        });
        if holds {
            owning.insert(definition.name.clone());
        }
    }
    owning.extend(aliases);
    owning
}

/// A `struct` or a `type` alias, with the identifiers of its definition.
struct TypeDefinition {
    name: String,
    alias: bool,
    idents: Vec<String>,
}

/// Every `struct` and `type` alias in `tokens`, outside test items.
fn collect_type_definitions(tokens: &[Token], out: &mut Vec<TypeDefinition>) {
    let mut i = 0;
    let mut skip_next_item = false;
    while i < tokens.len() {
        if tokens[i].is_punct('#')
            && let Some(attribute) = tokens.get(i + 1).and_then(|t| t.group(Delimiter::Bracket))
        {
            skip_next_item |= is_test_attribute(attribute);
            i += 2;
            continue;
        }
        let keyword = tokens[i].ident();
        if skip_next_item {
            while i < tokens.len() {
                let done = tokens[i].is_punct(';') || tokens[i].group(Delimiter::Brace).is_some();
                i += 1;
                if done {
                    break;
                }
            }
            skip_next_item = false;
            continue;
        }
        if matches!(keyword, Some("struct") | Some("type"))
            && let Some(name) = tokens.get(i + 1).and_then(Token::ident)
        {
            let mut idents = Vec::new();
            let mut j = i + 2;
            while j < tokens.len() {
                match &tokens[j].kind {
                    Kind::Punct(';') => break,
                    Kind::Group(Delimiter::Brace, inner) => {
                        idents.extend(flatten_idents(inner).into_iter().map(str::to_string));
                        break;
                    }
                    Kind::Group(_, inner) => {
                        idents.extend(flatten_idents(inner).into_iter().map(str::to_string));
                    }
                    Kind::Ident(ident) => idents.push(ident.clone()),
                    _ => {}
                }
                j += 1;
            }
            out.push(TypeDefinition {
                name: name.to_string(),
                alias: keyword == Some("type"),
                idents,
            });
            i = j + 1;
            continue;
        }
        if let Kind::Group(Delimiter::Brace, inner) = &tokens[i].kind {
            collect_type_definitions(inner, out);
        }
        i += 1;
    }
}

/// Walk one item-level token sequence, collecting every `fn` with a body.
fn collect_items(
    tokens: &[Token],
    file: usize,
    impl_idents: &[String],
    context: &Context<'_>,
    out: &mut Vec<FnDef>,
) {
    let mut i = 0;
    let mut skip_next_item = false;
    while i < tokens.len() {
        let token = &tokens[i];
        if token.is_punct('#') {
            let mut j = i + 1;
            if tokens.get(j).is_some_and(|t| t.is_punct('!')) {
                j += 1;
            }
            if let Some(attribute) = tokens.get(j).and_then(|t| t.group(Delimiter::Bracket)) {
                if is_test_attribute(attribute) {
                    skip_next_item = true;
                }
                i = j + 1;
                continue;
            }
        }
        if skip_next_item {
            // The gated item ends at its first brace body or its `;`.
            while i < tokens.len() {
                let done = tokens[i].is_punct(';') || tokens[i].group(Delimiter::Brace).is_some();
                i += 1;
                if done {
                    break;
                }
            }
            skip_next_item = false;
            continue;
        }
        match token.ident() {
            Some("macro_rules") => {
                // A macro body is a token pattern, not code this walk can read.
                while i < tokens.len() && tokens[i].group(Delimiter::Brace).is_none() {
                    i += 1;
                }
                i += 1;
                continue;
            }
            Some("impl") => {
                let mut header = Vec::new();
                let mut j = i + 1;
                while j < tokens.len() && tokens[j].group(Delimiter::Brace).is_none() {
                    if let Some(name) = tokens[j].ident() {
                        header.push(name.to_string());
                    }
                    j += 1;
                }
                if let Some(body) = tokens.get(j).and_then(|t| t.group(Delimiter::Brace)) {
                    collect_items(body, file, &header, context, out);
                }
                i = j + 1;
                continue;
            }
            Some("mod") | Some("trait") => {
                let mut j = i + 1;
                while j < tokens.len()
                    && tokens[j].group(Delimiter::Brace).is_none()
                    && !tokens[j].is_punct(';')
                {
                    j += 1;
                }
                if let Some(body) = tokens.get(j).and_then(|t| t.group(Delimiter::Brace)) {
                    collect_items(body, file, impl_idents, context, out);
                }
                i = j + 1;
                continue;
            }
            Some("fn") => {
                if let Some(name) = tokens.get(i + 1).and_then(Token::ident) {
                    let mut signature = Vec::new();
                    let mut j = i + 2;
                    while j < tokens.len()
                        && tokens[j].group(Delimiter::Brace).is_none()
                        && !tokens[j].is_punct(';')
                    {
                        signature.push(tokens[j].clone());
                        j += 1;
                    }
                    if let Some(body) = tokens.get(j).and_then(|t| t.group(Delimiter::Brace)) {
                        let seed = replay_seed(&signature, impl_idents, context);
                        let mut def = FnDef {
                            name: name.to_string(),
                            file,
                            seed,
                            reads: Vec::new(),
                            unrecorded_calls: BTreeSet::new(),
                        };
                        walk_body(body, false, context.surface, &mut def);
                        out.push(def);
                        // Items nested in the body (inner fns) are items too.
                        collect_items(body, file, &[], context, out);
                    }
                    i = j + 1;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

/// Why a `fn` with this signature, in an impl of `impl_idents`, is on a
/// replay path: it names a controller or journal context, or it belongs to
/// a type that owns one.
fn replay_seed(
    signature: &[Token],
    impl_idents: &[String],
    context: &Context<'_>,
) -> Option<String> {
    let idents = flatten_idents(signature);
    if let Some(ty) = idents.iter().find(|ident| {
        context.surface.replay_signature_types.contains(ident) || context.owning.contains(**ident)
    }) {
        return Some(format!("its signature names `{ty}`"));
    }
    // A Restate handler's own context; a `Poll`-shaped signature is the
    // std task context of a hand-written future instead.
    if context.restate && idents.contains(&"Context") && !idents.contains(&"Poll") {
        return Some("its signature names a Restate `Context`".to_string());
    }
    impl_idents
        .iter()
        .find(|name| context.owning.contains(*name))
        .map(|ty| format!("it is a method of `{ty}`, a replay-path type"))
}

/// Walk a body, recording surface reads (and whether a recorded step's span
/// contains each) and the names called outside every step.
fn walk_body(tokens: &[Token], in_step: bool, surface: &Surface<'_>, def: &mut FnDef) {
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        // A nested `fn` item is its own definition.
        if token.ident() == Some("fn") && tokens.get(i + 1).and_then(Token::ident).is_some() {
            let mut j = i + 2;
            while j < tokens.len()
                && tokens[j].group(Delimiter::Brace).is_none()
                && !tokens[j].is_punct(';')
            {
                j += 1;
            }
            i = j + 1;
            continue;
        }
        if let Some(name) = token.ident() {
            let method = i > 0 && tokens[i - 1].is_punct('.');
            let receiver = if method && i > 1 {
                tokens[i - 2].ident()
            } else {
                None
            };
            let (arguments, next) = call_arguments(tokens, i + 1);
            if let Some(arguments) = arguments {
                let step = surface.steps.contains(&name)
                    || (method && name == "run" && matches!(receiver, Some("ctx")));
                if step {
                    walk_body(arguments, true, surface, def);
                } else {
                    if surface.reads.contains(&name) {
                        def.reads.push(ReadSite {
                            method: name.to_string(),
                            line: token.line,
                            recorded: in_step,
                        });
                    } else if !in_step && (!method || receiver == Some("self")) {
                        // A free call or a call on `self` names a function
                        // of this code; a method on any other receiver may
                        // be anything, so it carries no replay path by name.
                        def.unrecorded_calls.insert(name.to_string());
                    }
                    walk_body(arguments, in_step, surface, def);
                }
                i = next;
                continue;
            }
        }
        if let Kind::Group(_, inner) = &token.kind {
            walk_body(inner, in_step, surface, def);
        }
        i += 1;
    }
}

/// If a call's argument list follows at `at` (directly, or after a
/// turbofish), that list and the index just past it.
fn call_arguments(tokens: &[Token], at: usize) -> (Option<&[Token]>, usize) {
    let mut j = at;
    if tokens.get(j).is_some_and(|t| t.is_punct(':'))
        && tokens.get(j + 1).is_some_and(|t| t.is_punct(':'))
        && tokens.get(j + 2).is_some_and(|t| t.is_punct('<'))
    {
        let mut depth = 0usize;
        j += 2;
        while j < tokens.len() {
            if tokens[j].is_punct('<') {
                depth += 1;
            } else if tokens[j].is_punct('>') && !tokens[j - 1].is_punct('-') {
                depth -= 1;
                if depth == 0 {
                    j += 1;
                    break;
                }
            }
            j += 1;
        }
    }
    match tokens.get(j).and_then(|t| t.group(Delimiter::Paren)) {
        Some(arguments) => (Some(arguments), j + 1),
        None => (None, at),
    }
}

/// Every `fn` name declared inside `trait <name>` in `source`.
pub(super) fn trait_methods(source: &str, trait_name: &str) -> Result<Vec<String>, String> {
    let tokens = tokenize(source)?;
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].ident() == Some("trait")
            && tokens.get(i + 1).and_then(Token::ident) == Some(trait_name)
        {
            let mut j = i + 2;
            while j < tokens.len() && tokens[j].group(Delimiter::Brace).is_none() {
                j += 1;
            }
            let Some(body) = tokens.get(j).and_then(|t| t.group(Delimiter::Brace)) else {
                break;
            };
            let mut names = Vec::new();
            for (k, token) in body.iter().enumerate() {
                if token.ident() == Some("fn")
                    && let Some(name) = body.get(k + 1).and_then(Token::ident)
                {
                    names.push(name.to_string());
                }
            }
            return Ok(names);
        }
        i += 1;
    }
    Err(format!("trait `{trait_name}` is not declared"))
}
