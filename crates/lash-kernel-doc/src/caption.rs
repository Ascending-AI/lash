//! Plain-language captions for a document's steps, its entries' parameters
//! and the fields of their types: a value beside the document, keyed to its
//! identity like a source map, that a dialect reads from and writes into
//! source and a host holds (`K-CAP-001`..`K-CAP-007`).
//!
//! A caption never changes behaviour or the document's identity. Function
//! values and saved function metadata carry source captions as data; they
//! do not alter executable documents or continuations.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Correspondence, Document, DocumentId, IdentifiedDocument, Node, Stmt};
use lash_kernel_value::sites::Site;
use lash_kernel_value::sites::Unit;

mod target;

pub use target::{CaptionEntry, CaptionTarget, FieldPath, FieldRoot, FieldTarget, ParamTarget};

/// The longest title, in UTF-8 bytes.
pub const CAPTION_TITLE_BYTES: usize = 200;
/// The longest description, in UTF-8 bytes.
pub const CAPTION_DESCRIPTION_BYTES: usize = 2048;

/// A step's title and optional description, in the one text domain every
/// dialect can spell and read back exactly (`K-CAP-005`). A value exists only
/// once [`Caption::new`] accepted its text; decoding checks it the same way.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Caption {
    /// One non-empty, trimmed line of at most [`CAPTION_TITLE_BYTES`].
    title: String,
    /// At most [`CAPTION_DESCRIPTION_BYTES`] of trimmed lines separated by
    /// `\n`, with no blank first or last line and no two blank lines in a
    /// row. Never empty: no description is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

/// Which text of a caption a refusal names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CaptionField {
    Title,
    Description,
}

impl std::fmt::Display for CaptionField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Title => "title",
            Self::Description => "description",
        })
    }
}

/// Why text is not a caption: the first failed check of the title, then of
/// the description, each in the order the variants are declared.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CaptionRefusal {
    #[error("a caption's title is empty")]
    EmptyTitle,
    /// `Some("")`: no description is `None`.
    #[error("a caption's description is empty; omit it instead")]
    EmptyDescription,
    #[error("a caption's {field} is longer than {limit} bytes")]
    TooLong { field: CaptionField, limit: usize },
    /// `\r`, U+2028 or U+2029: lines are separated by `\n` alone.
    #[error("a caption's {field} holds the line separator U+{:04X}", u32::from(*character))]
    LineSeparator {
        field: CaptionField,
        character: char,
    },
    /// A C0 control character other than a description's `\n`.
    #[error("a caption's {field} holds the control character U+{:04X}", u32::from(*character))]
    Control {
        field: CaptionField,
        character: char,
    },
    #[error("a caption's title is more than one line")]
    MultilineTitle,
    #[error("a line of a caption's {field} starts or ends with whitespace")]
    Untrimmed { field: CaptionField },
    #[error("a caption's description starts or ends with a blank line")]
    BlankEdge,
    #[error("a caption's description has two blank lines in a row")]
    BlankRun,
    /// `*/`, which would end a block comment around the caption.
    #[error("a caption's {field} holds `*/`")]
    CommentClose { field: CaptionField },
    /// A line starting with `@`, which a documentation comment reads as a tag.
    #[error("a line of a caption's {field} starts with `@`")]
    TagLine { field: CaptionField },
}

impl Caption {
    /// A caption whose text the rule admits.
    ///
    /// # Errors
    ///
    /// The first [`CaptionRefusal`] the title, then the description, fails.
    pub fn new(
        title: impl Into<String>,
        description: Option<String>,
    ) -> Result<Self, CaptionRefusal> {
        let caption = Self {
            title: title.into(),
            description,
        };
        caption.check()?;
        Ok(caption)
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// The UTF-8 bytes of its title and description.
    #[must_use]
    pub fn text_bytes(&self) -> usize {
        self.title.len() + self.description.as_ref().map_or(0, String::len)
    }

    fn check(&self) -> Result<(), CaptionRefusal> {
        check_field(&self.title, CaptionField::Title)?;
        match &self.description {
            Some(description) => check_field(description, CaptionField::Description),
            None => Ok(()),
        }
    }
}

fn check_field(text: &str, field: CaptionField) -> Result<(), CaptionRefusal> {
    let limit = match field {
        CaptionField::Title => CAPTION_TITLE_BYTES,
        CaptionField::Description => CAPTION_DESCRIPTION_BYTES,
    };
    if text.is_empty() {
        return Err(match field {
            CaptionField::Title => CaptionRefusal::EmptyTitle,
            CaptionField::Description => CaptionRefusal::EmptyDescription,
        });
    }
    if text.len() > limit {
        return Err(CaptionRefusal::TooLong { field, limit });
    }
    for character in text.chars() {
        if matches!(character, '\r' | '\u{2028}' | '\u{2029}') {
            return Err(CaptionRefusal::LineSeparator { field, character });
        }
        if character < ' ' && character != '\n' {
            return Err(CaptionRefusal::Control { field, character });
        }
    }
    if field == CaptionField::Title && text.contains('\n') {
        return Err(CaptionRefusal::MultilineTitle);
    }
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.iter().any(|line| line.trim() != *line) {
        return Err(CaptionRefusal::Untrimmed { field });
    }
    // A trimmed line is blank exactly when it is empty.
    if lines.first().is_some_and(|line| line.is_empty())
        || lines.last().is_some_and(|line| line.is_empty())
    {
        return Err(CaptionRefusal::BlankEdge);
    }
    if lines
        .windows(2)
        .any(|pair| pair[0].is_empty() && pair[1].is_empty())
    {
        return Err(CaptionRefusal::BlankRun);
    }
    if text.contains("*/") {
        return Err(CaptionRefusal::CommentClose { field });
    }
    if lines.iter().any(|line| line.starts_with('@')) {
        return Err(CaptionRefusal::TagLine { field });
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptionWire {
    title: String,
    #[serde(default)]
    description: Option<String>,
}

impl<'de> Deserialize<'de> for Caption {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = CaptionWire::deserialize(deserializer)?;
        Self::new(wire.title, wire.description).map_err(serde::de::Error::custom)
    }
}

/// What a caption may name (`K-CAP-003`, `K-CAP-007`): a captionable site,
/// a declared function's parameter or a field of an entry's types. A
/// then-arm, a loop body, a try body, a closure body and `main`'s body are
/// not captionable: the statement or callable that holds them names them.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CaptionKind {
    /// A statement of `main` or of a declared function.
    Statement,
    /// An `if`'s else block, empty or not.
    ElseArm,
    /// A `try`'s catch block.
    CatchArm,
    /// A `try`'s finally block.
    FinallyArm,
    /// A loop's non-empty else block.
    LoopElseArm,
    /// A declared function's body, at `Site(Function(f), [])`: the entry's
    /// is the workflow's title.
    Function,
    /// A declared function's parameter ([`CaptionTarget::Param`]).
    Param,
    /// A named field of an entry's parameter or result type
    /// ([`CaptionTarget::Field`]).
    Field,
}

impl CaptionKind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 8] = [
        Self::Statement,
        Self::ElseArm,
        Self::CatchArm,
        Self::FinallyArm,
        Self::LoopElseArm,
        Self::Function,
        Self::Param,
        Self::Field,
    ];

    /// What `site` is in `document`, when a caption may name it.
    #[must_use]
    pub fn of(document: &Document, site: &Site) -> Option<Self> {
        let Some((&index, parent)) = site.path.split_last() else {
            return match &site.unit {
                Unit::Library(id) if document.functions.contains_key(id) => Some(Self::Function),
                _ => None,
            };
        };
        let parent = document.node(&Site::new(site.unit.clone(), parent))?;
        match (parent, document.node(site)?) {
            (_, Node::Stmt(_)) => Some(Self::Statement),
            (Node::Stmt(statement), Node::Block(_)) => Self::arm(statement, index),
            _ => None,
        }
    }

    /// The block child `index` of `statement` as an arm.
    fn arm(statement: &Stmt, index: u32) -> Option<Self> {
        match statement {
            Stmt::If { .. } if index == 2 => Some(Self::ElseArm),
            Stmt::For { .. } | Stmt::While { .. } if index == 2 => Some(Self::LoopElseArm),
            Stmt::Try(scope) => {
                let catch = u32::from(scope.catch.is_some());
                if scope.catch.is_some() && index == 1 {
                    Some(Self::CatchArm)
                } else if scope.finally.is_some() && index == 1 + catch {
                    Some(Self::FinallyArm)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Every target of `document` a caption may name, ascending: its
    /// captionable sites, every parameter of a declared function and every
    /// field path of an entry's signature.
    #[must_use]
    pub fn captionable(document: &Document) -> Vec<(CaptionTarget, Self)> {
        let mut out: Vec<(CaptionTarget, Self)> = Self::captionable_sites(document)
            .into_iter()
            .map(|(site, kind)| (CaptionTarget::Site(site), kind))
            .collect();
        for (name, export) in &document.exports {
            let crate::Export::Function(function) = export else {
                continue;
            };
            for param in &function.signature.params {
                out.push((
                    CaptionTarget::Param(ParamTarget {
                        function: name.clone(),
                        param: param.name.clone(),
                    }),
                    Self::Param,
                ));
            }
            out.extend(
                FieldPath::every(&function.signature)
                    .into_iter()
                    .map(|path| {
                        (
                            CaptionTarget::Field(FieldTarget {
                                function: name.clone(),
                                path,
                            }),
                            Self::Field,
                        )
                    }),
            );
        }
        out.sort();
        out
    }

    /// Every captionable site of `document`, ascending.
    fn captionable_sites(document: &Document) -> Vec<(Site, Self)> {
        let mut out = Vec::new();
        let roots = document.units();
        for (unit, body) in roots {
            let root = Site::new(unit, Vec::new());
            if matches!(root.unit, Unit::Library(_)) {
                out.push((root.clone(), Self::Function));
            }
            let mut pending = vec![(Node::Block(body), root)];
            while let Some((node, site)) = pending.pop() {
                for (index, child) in (0u32..).zip(node.children()) {
                    let child_site = site.child(index);
                    let kind = match (node, child) {
                        (_, Node::Stmt(_)) => Some(Self::Statement),
                        (Node::Stmt(statement), Node::Block(_)) => Self::arm(statement, index),
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        out.push((child_site.clone(), kind));
                    }
                    pending.push((child, child_site));
                }
            }
        }
        out.sort();
        out
    }
}

/// The captions of one document, by target: strictly ascending and unique,
/// each a target the document of identity `document` lets a caption name. It is
/// separate from the [`crate::SourceMap`]: captions outlive the source bytes
/// a map is keyed to. A host holds workflow captions; extracted cell function
/// values carry their statement captions.
///
/// Decoding checks the order; validate a decoded value against its document
/// before using its targets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Captions {
    document: DocumentId,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    entries: Vec<CaptionEntry>,
}

/// The first failed check: document, then each entry's target and kind in
/// order, then ordering.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CaptionsRefusal {
    #[error("captions name another document")]
    Document,
    #[error("caption site {site} does not exist")]
    Site { site: Site },
    #[error("caption site {site} is not a statement, a clause arm or a declared function")]
    NotCaptionable { site: Site },
    /// The function is not declared, or declares no parameter of the name.
    #[error("fn {} has no parameter {}", .target.function, .target.param)]
    Param { target: ParamTarget },
    /// The function is no entry, or the path does not walk its signature's
    /// types to a named record field ([`FieldPath`]).
    #[error("{} does not name a field of the types fn {} states", .target.path, .target.function)]
    Field { target: FieldTarget },
    #[error("captions are unsorted at {target}")]
    Order { target: CaptionTarget },
    #[error("two captions name {target}")]
    Duplicate { target: CaptionTarget },
}

/// What [`Captions::rekey`] answers: the survivors at their new targets,
/// and which of them an edit may have made stale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rekeyed {
    pub captions: Captions,
    /// The sites in `captions` whose node or any node beneath it the edit
    /// wrote or made new, ascending. A host marks those captions for review.
    /// A parameter's or a field's caption is never touched: it survives
    /// only while its name does.
    pub touched: Vec<Site>,
}

impl Captions {
    /// No captions of the document of identity `document`.
    #[must_use]
    pub fn empty(document: DocumentId) -> Self {
        Self {
            document,
            entries: Vec::new(),
        }
    }

    /// Build the one canonical ordering, then check every target.
    ///
    /// # Errors
    ///
    /// Two entries for one target, or a target `document` does not hold or
    /// a caption may not name.
    pub fn new(document: &Document, entries: Vec<CaptionEntry>) -> Result<Self, CaptionsRefusal> {
        Self::of(document, document.identity(), entries)
    }

    /// [`Self::new`] for a document whose identity is already taken.
    ///
    /// # Errors
    ///
    /// As [`Self::new`].
    pub fn new_identified(
        document: &IdentifiedDocument,
        entries: Vec<CaptionEntry>,
    ) -> Result<Self, CaptionsRefusal> {
        Self::of(document, document.id(), entries)
    }

    fn of(
        document: &Document,
        identity: DocumentId,
        mut entries: Vec<CaptionEntry>,
    ) -> Result<Self, CaptionsRefusal> {
        entries.sort_by(|a, b| a.target.cmp(&b.target));
        let captions = Self {
            document: identity,
            entries,
        };
        captions.check(document, identity)?;
        Ok(captions)
    }

    /// Check against the actual document.
    ///
    /// # Errors
    ///
    /// The first [`CaptionsRefusal`].
    pub fn validate(&self, document: &Document) -> Result<(), CaptionsRefusal> {
        self.check(document, document.identity())
    }

    /// [`Self::validate`] against a document whose identity is already
    /// taken: nothing is encoded.
    ///
    /// # Errors
    ///
    /// The first [`CaptionsRefusal`].
    pub fn validate_identified(
        &self,
        document: &IdentifiedDocument,
    ) -> Result<(), CaptionsRefusal> {
        self.check(document, document.id())
    }

    fn check(&self, document: &Document, identity: DocumentId) -> Result<(), CaptionsRefusal> {
        if self.document != identity {
            return Err(CaptionsRefusal::Document);
        }
        for entry in &self.entries {
            if entry.target.kind(document).is_some() {
                continue;
            }
            return Err(match &entry.target {
                CaptionTarget::Site(site) if document.node(site).is_none() => {
                    CaptionsRefusal::Site { site: site.clone() }
                }
                CaptionTarget::Site(site) => CaptionsRefusal::NotCaptionable { site: site.clone() },
                CaptionTarget::Param(target) => CaptionsRefusal::Param {
                    target: target.clone(),
                },
                CaptionTarget::Field(target) => CaptionsRefusal::Field {
                    target: target.clone(),
                },
            });
        }
        self.check_order()
    }

    fn check_order(&self) -> Result<(), CaptionsRefusal> {
        for pair in self.entries.windows(2) {
            match pair[0].target.cmp(&pair[1].target) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => {
                    return Err(CaptionsRefusal::Duplicate {
                        target: pair[1].target.clone(),
                    });
                }
                std::cmp::Ordering::Greater => {
                    return Err(CaptionsRefusal::Order {
                        target: pair[1].target.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// The identity of the document the captions name.
    #[must_use]
    pub fn document(&self) -> DocumentId {
        self.document
    }

    /// The captions, ascending by target.
    #[must_use]
    pub fn entries(&self) -> &[CaptionEntry] {
        &self.entries
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The caption of `target`, when it has one.
    #[must_use]
    pub fn get(&self, target: &CaptionTarget) -> Option<&Caption> {
        self.entries
            .binary_search_by(|entry| entry.target.cmp(target))
            .ok()
            .map(|index| &self.entries[index].caption)
    }

    /// The caption of `site`, when it has one.
    #[must_use]
    pub fn at(&self, site: &Site) -> Option<&Caption> {
        self.entries
            .binary_search_by(|entry| match &entry.target {
                CaptionTarget::Site(at) => at.cmp(site),
                CaptionTarget::Param(_) | CaptionTarget::Field(_) => std::cmp::Ordering::Greater,
            })
            .ok()
            .map(|index| &self.entries[index].caption)
    }

    /// The UTF-8 bytes of every caption's text.
    #[must_use]
    pub fn text_bytes(&self) -> usize {
        self.entries
            .iter()
            .map(|entry| entry.caption.text_bytes())
            .sum()
    }

    /// Move surviving captions to their targets in `result`, the document
    /// the correspondence leads to, as [`crate::SourceMap::rekey`] moves
    /// locations. A site's caption follows its node. A parameter's or a
    /// field's follows its named Export's exact body and keeps its name: it survives
    /// exactly while that function still has the parameter, or its
    /// signature still resolves the path, and no edit touches it. A
    /// survivor that is no longer captionable there is dropped with the
    /// rest. `None` when the correspondence does not start at these
    /// captions' document or does not end at `result`.
    #[must_use]
    pub fn rekey(
        &self,
        correspondence: &Correspondence,
        base: &Document,
        result: &Document,
    ) -> Option<Rekeyed> {
        if correspondence.base != self.document
            || base.identity() != self.document
            || correspondence.result != result.identity()
        {
            return None;
        }
        let mut entries: Vec<_> = self
            .entries
            .iter()
            .filter_map(|entry| {
                let target = match &entry.target {
                    CaptionTarget::Site(site) => {
                        CaptionTarget::Site(correspondence.successor(site)?.clone())
                    }
                    other => {
                        let name = other.function()?;
                        let crate::Export::Function(before) = base.exports.get(name)? else {
                            return None;
                        };
                        let crate::Export::Function(after) = result.exports.get(name)? else {
                            return None;
                        };
                        let body = Site::new(Unit::Library(before.function), []);
                        let successor = Site::new(Unit::Library(after.function), []);
                        (correspondence.successor(&body)? == &successor).then(|| other.clone())?
                    }
                };
                target.kind(result)?;
                Some(CaptionEntry {
                    target,
                    caption: entry.caption.clone(),
                })
            })
            .collect();
        entries.sort_by(|a, b| a.target.cmp(&b.target));
        let survivors: BTreeSet<&Site> = correspondence
            .entries()
            .iter()
            .map(|entry| &entry.to)
            .collect();
        let edited: BTreeSet<&Site> = correspondence
            .entries()
            .iter()
            .filter(|entry| entry.edited)
            .map(|entry| &entry.to)
            .collect();
        let touched = entries
            .iter()
            .filter_map(|entry| entry.target.site())
            .filter(|site| written_at_or_beneath(result, site, &survivors, &edited))
            .cloned()
            .collect();
        Some(Rekeyed {
            captions: Self {
                document: correspondence.result,
                entries,
            },
            touched,
        })
    }
}

/// Whether an edit wrote the node at `site` or anything beneath it: an
/// edited survivor, or a node no survivor leads to.
fn written_at_or_beneath(
    document: &Document,
    site: &Site,
    survivors: &BTreeSet<&Site>,
    edited: &BTreeSet<&Site>,
) -> bool {
    let Some(node) = document.node(site) else {
        return false;
    };
    let mut pending = vec![(node, site.clone())];
    while let Some((node, site)) = pending.pop() {
        if edited.contains(&site) || !survivors.contains(&site) {
            return true;
        }
        pending.extend(
            (0u32..)
                .zip(node.children())
                .map(|(index, child)| (child, site.child(index))),
        );
    }
    false
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptionsWire {
    document: DocumentId,
    #[serde(default)]
    entries: Vec<CaptionEntry>,
}

impl<'de> Deserialize<'de> for Captions {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = CaptionsWire::deserialize(deserializer)?;
        let captions = Self {
            document: wire.document,
            entries: wire.entries,
        };
        captions.check_order().map_err(serde::de::Error::custom)?;
        Ok(captions)
    }
}

impl lash_kernel_value::tokens::TokenCodec for Caption {
    fn encode(&self, out: &mut lash_kernel_value::tokens::TokenWriter) {
        out.seq(2);
        out.text(&self.title);
        self.description.encode(out);
    }

    fn decode(
        input: &mut lash_kernel_value::tokens::TokenReader<'_>,
    ) -> Result<Self, lash_kernel_value::tokens::TokenError> {
        input.expect_struct(2)?;
        let title = String::decode(input)?;
        let description = Option::decode(input)?;
        Self::new(title, description)
            .map_err(|error| lash_kernel_value::tokens::TokenError::Invalid(error.to_string()))
    }
}

impl lash_kernel_value::tokens::TokenCodec for Captions {
    fn encode(&self, out: &mut lash_kernel_value::tokens::TokenWriter) {
        out.seq(2);
        self.document.encode(out);
        self.entries.encode(out);
    }

    fn decode(
        input: &mut lash_kernel_value::tokens::TokenReader<'_>,
    ) -> Result<Self, lash_kernel_value::tokens::TokenError> {
        input.expect_struct(2)?;
        let captions = Self {
            document: DocumentId::decode(input)?,
            entries: Vec::decode(input)?,
        };
        captions
            .check_order()
            .map_err(|error| lash_kernel_value::tokens::TokenError::Invalid(error.to_string()))?;
        Ok(captions)
    }
}
