//! Where the captions of generated source go. The reader keeps no Sites:
//! it numbers statements in the order it begins them, which is the order a
//! walk of the document meets them, and finds each caption's Site by its
//! statement's number once the document is whole.

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

    /// The captions of `document`, as read, ascending. One that names no
    /// captionable site, such as `main`'s, is an ordinary comment.
    pub(super) fn resolve(self, document: &k::Document) -> Vec<k::CaptionEntry> {
        if self.targets.is_empty() {
            return Vec::new();
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
        let mut entries: Vec<k::CaptionEntry> = self
            .targets
            .into_iter()
            .filter_map(|(target, caption)| {
                let site = match target {
                    Target::Statement(ordinal) => sites.get(ordinal)?.clone(),
                    Target::Arm(owner, child) => sites.get(owner)?.child(child),
                    Target::Function(name) => k::Site::new(k::Unit::Function(name), Vec::new()),
                };
                k::CaptionKind::of(document, &site)?;
                Some(k::CaptionEntry { site, caption })
            })
            .collect();
        entries.sort_by(|left, right| left.site.cmp(&right.site));
        entries
    }
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
