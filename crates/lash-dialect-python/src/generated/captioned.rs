//! Where the captions of generated source go. The reader keeps no Sites:
//! it numbers statements in the order it begins them, which is the order a
//! walk of the document meets them, and finds each caption's Site by its
//! statement's number once the document is whole. A parameter's caption is
//! its function's and its name's, and a field's the path through the
//! `TypedDict`s a parameter's or the result's annotation names.

use std::collections::BTreeMap;

use lash_kernel_doc as k;

/// The lines of `source` as [`str::lines`] splits them, each with the byte
/// offset where it starts.
pub(super) fn lines(source: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut start = 0;
    source.split_inclusive('\n').map(move |raw| {
        let at = start;
        start += raw.len();
        let line = raw
            .strip_suffix('\n')
            .map_or(raw, |line| line.strip_suffix('\r').unwrap_or(line));
        (at, line)
    })
}

/// The byte ranges of the comment lines of generated source, which writes
/// no comment after code.
pub(super) fn comments(source: &str) -> impl Iterator<Item = std::ops::Range<usize>> + '_ {
    lines(source).filter_map(|(at, line)| {
        let comment = line.trim_start();
        comment
            .starts_with('#')
            .then(|| at + line.len() - comment.len()..at + line.len())
    })
}

/// What a caption the reader met names.
enum Target {
    /// The statement of this number.
    Statement(usize),
    /// Block child `.1` of the statement of number `.0`.
    Arm(usize, u32),
    Function(k::Name),
}

/// The captions met while reading, by what they name.
#[derive(Default)]
pub(super) struct Found {
    statements: usize,
    /// The bodies read, in the order they were.
    roots: Vec<k::Unit>,
    targets: Vec<(Target, k::Caption)>,
    /// The captions of parameters and fields.
    declared: Vec<k::CaptionEntry>,
}

/// One field of a `TypedDict` read: its name, its annotation as written
/// and the caption above it.
pub(super) struct ClassField {
    pub(super) name: String,
    pub(super) annotation: String,
    pub(super) caption: Option<k::Caption>,
}

/// The `TypedDict`s read, by name.
pub(super) type Classes = BTreeMap<String, Vec<ClassField>>;

/// One parameter of a signature read, one a line: the caption above it and
/// its annotation as written.
pub(super) struct WrittenParam {
    pub(super) caption: Option<k::Caption>,
    pub(super) annotation: Option<String>,
}

impl Found {
    /// Numbers the statement the reader begins, which `caption` names.
    pub(super) fn statement(&mut self, caption: Option<k::Caption>) -> usize {
        let ordinal = self.statements;
        self.statements += 1;
        if let Some(caption) = caption {
            self.targets.push((Target::Statement(ordinal), caption));
        }
        ordinal
    }

    pub(super) fn arm(&mut self, owner: usize, child: u32, caption: Option<k::Caption>) {
        if let Some(caption) = caption {
            self.targets.push((Target::Arm(owner, child), caption));
        }
    }

    /// Begins the body of `unit`, whose `def` `caption` names.
    pub(super) fn root(&mut self, unit: k::Unit, caption: Option<k::Caption>) {
        if let (k::Unit::Function(name), Some(caption)) = (&unit, caption) {
            self.targets.push((Target::Function(name.clone()), caption));
        }
        self.roots.push(unit);
    }

    /// Notes the captions of declared function `function`'s parameters,
    /// named `params`, as `written`, and of the fields of the `TypedDict`s
    /// their annotations and `result`'s name.
    pub(super) fn signature(
        &mut self,
        function: &k::Name,
        params: &[k::Name],
        written: Vec<WrittenParam>,
        result: Option<&str>,
        classes: &Classes,
    ) {
        let field =
            |root: k::FieldRoot, (fields, caption): (Vec<String>, k::Caption)| k::CaptionEntry {
                target: k::CaptionTarget::Field(k::FieldTarget {
                    function: function.clone(),
                    path: k::FieldPath { root, fields },
                }),
                caption,
            };
        for (param, written) in params.iter().zip(written) {
            if let Some(caption) = written.caption {
                self.declared.push(k::CaptionEntry {
                    target: k::CaptionTarget::Param(k::ParamTarget {
                        function: function.clone(),
                        param: param.clone(),
                    }),
                    caption,
                });
            }
            if let Some(annotation) = &written.annotation {
                self.declared.extend(
                    class_fields(annotation, classes)
                        .into_iter()
                        .map(|found| field(k::FieldRoot::Param(param.clone()), found)),
                );
            }
        }
        if let Some(result) = result {
            self.declared.extend(
                class_fields(result, classes)
                    .into_iter()
                    .map(|found| field(k::FieldRoot::Result, found)),
            );
        }
    }

    /// The captions of `document`, as read, ascending. One that names no
    /// captionable target, such as `main`'s, is an ordinary comment.
    pub(super) fn resolve(self, document: &k::Document) -> Vec<k::CaptionEntry> {
        let mut entries: Vec<k::CaptionEntry> = self
            .declared
            .into_iter()
            .filter(|entry| entry.target.kind(document).is_some())
            .collect();
        if self.targets.is_empty() {
            entries.sort_by(|left, right| left.target.cmp(&right.target));
            entries.dedup_by(|later, first| later.target == first.target);
            return entries;
        }
        let mut sites = Vec::with_capacity(self.statements);
        for unit in &self.roots {
            let body = match unit {
                k::Unit::Main => &document.main,
                k::Unit::Function(name) => match document.functions.get(name) {
                    Some(function) => &function.body,
                    None => continue,
                },
                k::Unit::Library(_) => continue,
            };
            statements(
                k::Node::Block(body),
                &k::Site::new(unit.clone(), Vec::new()),
                &mut sites,
            );
        }
        entries.extend(self.targets.into_iter().filter_map(|(target, caption)| {
            let site = match target {
                Target::Statement(ordinal) => sites.get(ordinal)?.clone(),
                Target::Arm(owner, child) => sites.get(owner)?.child(child),
                Target::Function(name) => k::Site::new(k::Unit::Function(name), Vec::new()),
            };
            k::CaptionKind::of(document, &site)?;
            Some(k::CaptionEntry::site(site, caption))
        }));
        entries.sort_by(|left, right| left.target.cmp(&right.target));
        entries.dedup_by(|later, first| later.target == first.target);
        entries
    }
}

/// The captions of the fields of the `TypedDict` `annotation` names, by
/// path: a class its annotation names alone, as a list's element or an
/// optional value is, is walked; one naming two is not.
fn class_fields(annotation: &str, classes: &Classes) -> Vec<(Vec<String>, k::Caption)> {
    fn walk<'c>(
        annotation: &str,
        classes: &'c Classes,
        path: &mut Vec<String>,
        open: &mut Vec<&'c str>,
        out: &mut Vec<(Vec<String>, k::Caption)>,
    ) {
        let mut named = words(annotation)
            .filter_map(|word| classes.get_key_value(word))
            .map(|(name, fields)| (name.as_str(), fields));
        let Some((name, fields)) = named.next() else {
            return;
        };
        if named.any(|(other, _)| other != name) || open.contains(&name) {
            return;
        }
        open.push(name);
        for field in fields {
            path.push(field.name.clone());
            if let Some(caption) = &field.caption {
                out.push((path.clone(), caption.clone()));
            }
            walk(&field.annotation, classes, path, open, out);
            path.pop();
        }
        open.pop();
    }
    let mut out = Vec::new();
    walk(
        annotation,
        classes,
        &mut Vec::new(),
        &mut Vec::new(),
        &mut out,
    );
    out
}

/// The identifiers of an annotation, outside its quoted texts.
fn words(annotation: &str) -> impl Iterator<Item = &str> {
    let mut quoted = false;
    let mut escaped = false;
    let mut start = None;
    let mut found = Vec::new();
    for (at, character) in annotation.char_indices() {
        if quoted {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            }
            continue;
        }
        let word = character == '_' || unicode_ident::is_xid_continue(character);
        match (word, start) {
            (true, None) => start = Some(at),
            (false, Some(from)) => {
                found.push(&annotation[from..at]);
                start = None;
            }
            _ => {}
        }
        quoted = character == '"';
    }
    if let Some(from) = start {
        found.push(&annotation[from..]);
    }
    found.into_iter()
}

/// The Sites of the statements under `node`, in the order a walk meets
/// them: each before what it holds.
fn statements(node: k::Node<'_>, site: &k::Site, out: &mut Vec<k::Site>) {
    for (index, child) in (0u32..).zip(node.children()) {
        let child_site = site.child(index);
        if matches!(child, k::Node::Stmt(_)) {
            out.push(child_site.clone());
        }
        statements(child, &child_site, out);
    }
}
