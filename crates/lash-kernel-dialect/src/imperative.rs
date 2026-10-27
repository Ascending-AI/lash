//! Bounded final-text writing and optional Site collection for generated codecs.
use std::collections::BTreeMap;

use lash_kernel_doc::{Caption, CaptionTarget, Captions, Document, Node};

use lash_kernel_doc::{SourceLanguage, SourceMap, SourceSite, Span};

use {lash_kernel_value::sites::Site, lash_kernel_value::sites::Unit};

/// Generated source is a separate, disposable format from stored documents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFormat {
    TypeScriptV1,
    PythonV1,
}

impl SourceFormat {
    pub fn language(self) -> SourceLanguage {
        match self {
            Self::TypeScriptV1 => SourceLanguage::TypeScript,
            Self::PythonV1 => SourceLanguage::Python,
        }
    }

    /// The installed dialect that owns this generated format.
    pub fn dialect(self) -> &'static str {
        match self {
            Self::TypeScriptV1 => "typescript",
            Self::PythonV1 => "python",
        }
    }
}

/// Host policy for whole generated workflows, independent of cell limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GeneratedSourceLimits {
    pub max_bytes: usize,
    /// Node depth, counting the root block as zero. Native safety ceiling: 64.
    pub max_depth: usize,
}
impl Default for GeneratedSourceLimits {
    fn default() -> Self {
        Self {
            max_bytes: 16 * 1024 * 1024,
            max_depth: 64,
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrintOptions {
    pub source_map: bool,
    pub limits: GeneratedSourceLimits,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrintedSource {
    pub source: String,
    pub format: SourceFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_map: Option<SourceMap>,
    /// The captions the dialect could not spell, ascending by target: every
    /// one of a kind its [`crate::CaptionSupport`] lacks, and any its syntax
    /// has no place for (`K-CAP-006`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<CaptionTarget>,
}
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PrintError {
    #[error(transparent)]
    Namespace(#[from] crate::NamespaceRefusal),
    #[error("generated source exceeds byte budget {limit}")]
    Bytes { limit: usize },
    #[error("generated source exceeds depth budget {limit}")]
    Depth { limit: usize },
    #[error("cannot encode generated source: {0}")]
    Encode(#[from] lash_kernel_doc::EncodeError),
    /// Authored syntax cannot represent this callable or main region.
    #[error("no authored source representation at {site}")]
    Unprintable { site: Site },
    #[error("invalid generated source: {0}")]
    Invalid(String),
}
impl From<PrintError> for crate::Diagnostic {
    fn from(error: PrintError) -> Self {
        Self::dialect(
            crate::DialectCode::from_static(match &error {
                PrintError::Namespace(_) => "GENERATED_SOURCE_NAMESPACE",
                PrintError::Bytes { .. } => "GENERATED_SOURCE_BYTES",
                PrintError::Depth { .. } => "GENERATED_SOURCE_DEPTH",
                PrintError::Unprintable { .. } => "GENERATED_SOURCE_UNPRINTABLE",
                PrintError::Encode(_) | PrintError::Invalid(_) => "GENERATED_SOURCE_INVALID",
            }),
            crate::DiagnosticKind::Refusal,
            None,
            error.to_string(),
        )
    }
}

/// Refuse depth before any recursive conversion, rendering, hashing or encoding.
/// The iterative walk is ordinary structural work, also required without maps.
pub fn check_document_depth(
    document: &Document,
    limits: GeneratedSourceLimits,
) -> Result<(), PrintError> {
    let limit = limits.max_depth.min(64);
    for import in document.imports.values() {
        check_leaf_size(
            import.name.package.to_string().len() + import.name.member.as_str().len(),
            limits.max_bytes,
        )?;
    }
    let roots = document.main.iter().map(|block| Node::Block(block)).chain(
        document
            .functions
            .values()
            .filter_map(|function| function.body())
            .map(|body| Node::Block(&body.block)),
    );
    for root in roots {
        let mut stack = vec![(root, 0usize)];
        while let Some((node, depth)) = stack.pop() {
            if depth > limit {
                return Err(PrintError::Depth { limit });
            }
            match node {
                Node::Expr(lash_kernel_doc::Expr::Literal(literal)) => {
                    check_literal_size(literal, limits.max_bytes)?
                }
                Node::Action(action) => check_action_leaves(action, limits.max_bytes)?,
                _ => {}
            }
            if let Node::Action(lash_kernel_doc::Action::Perform { result, .. }) = node {
                check_type(result, limit)?;
            }
            stack.extend(node.children().into_iter().map(|child| (child, depth + 1)));
        }
    }
    for import in document.imports.values() {
        if let lash_kernel_value::ImportKind::Code { signature, .. }
        | lash_kernel_value::ImportKind::Operation { signature } = &import.kind
        {
            check_signature(signature, limit)?;
        }
    }
    for function in document.functions.values() {
        check_signature(&function.signature, limit)?;
    }
    // Count actual encoded input bytes before names or source buffers are allocated.
    // Depth has already been checked, so this serializer is stack bounded.
    let mut counter = ByteCounter {
        used: 0,
        limit: limits.max_bytes,
    };
    serde_json::to_writer(&mut counter, document).map_err(|_| PrintError::Bytes {
        limit: limits.max_bytes,
    })?;
    Ok(())
}
/// Refuse captions that do not validate against `document`, or whose text
/// alone exceeds the byte budget, before any source is written.
///
/// # Errors
///
/// [`PrintError::Invalid`] naming the captions' refusal, or
/// [`PrintError::Bytes`].
pub fn check_captions(
    document: &Document,
    captions: &Captions,
    limits: GeneratedSourceLimits,
) -> Result<(), PrintError> {
    captions
        .validate(document)
        .map_err(|refusal| PrintError::Invalid(refusal.to_string()))?;
    check_leaf_size(captions.text_bytes(), limits.max_bytes)
}
fn check_leaf_size(size: usize, limit: usize) -> Result<(), PrintError> {
    if size > limit {
        Err(PrintError::Bytes { limit })
    } else {
        Ok(())
    }
}
fn check_literal_size(literal: &lash_kernel_doc::Literal, limit: usize) -> Result<(), PrintError> {
    use lash_kernel_doc::Literal;
    match literal {
        Literal::Bytes(bytes) => check_leaf_size(
            bytes
                .as_slice()
                .len()
                .checked_mul(2)
                .ok_or(PrintError::Bytes { limit })?,
            limit,
        ),
        // More than four magnitude bits per admitted byte cannot fit in decimal.
        // A passing estimate bounds allocation by a constant multiple of the budget.
        // The exact encoded byte count is still enforced by ByteCounter.
        Literal::Int(integer) => check_leaf_size(
            usize::try_from(integer.bits() / 4).map_err(|_| PrintError::Bytes { limit })?,
            limit,
        ),
        Literal::Text(text) => check_leaf_size(text.len(), limit),
        Literal::Null
        | Literal::Absent
        | Literal::Bool(_)
        | Literal::Float(_)
        | Literal::Function(_) => Ok(()),
    }
}
fn check_action_leaves(action: &lash_kernel_doc::Action, limit: usize) -> Result<(), PrintError> {
    use lash_kernel_doc::{Action, Atom};

    let check = |atom: &Atom| {
        if let Atom::Literal(literal) = atom {
            check_literal_size(literal, limit)
        } else {
            Ok(())
        }
    };
    match action {
        Action::Call { args, .. } | Action::Spawn { args, .. } => {
            for atom in args {
                check(atom)?;
            }
        }
        Action::Perform { args, .. } => {
            for atom in args {
                check(atom)?;
            }
        }
        Action::Await { value, .. } => check(value)?,
        Action::Join { task } | Action::Cancel { task } => check(task)?,
        Action::JoinMany { tasks, .. } => check(tasks)?,
        Action::Yield => {}
    }
    Ok(())
}
fn check_signature(
    signature: &lash_kernel_value::Signature,
    limit: usize,
) -> Result<(), PrintError> {
    check_type(&signature.result, limit)?;
    for param in &signature.params {
        check_type(&param.ty, limit)?;
    }
    Ok(())
}
fn check_type(ty: &lash_kernel_value::Type, limit: usize) -> Result<(), PrintError> {
    use lash_kernel_value::Type;
    let mut stack = vec![(ty, 1usize)];
    while let Some((ty, depth)) = stack.pop() {
        if depth > limit {
            return Err(PrintError::Depth { limit });
        }
        let next = depth + 1;
        match ty {
            Type::Tuple(items) | Type::Union(items) => {
                stack.extend(items.iter().map(|t| (t, next)))
            }
            Type::List(t) | Type::Set(t) | Type::Task(t) => stack.push((t, next)),
            Type::Map(t) => {
                stack.push((&t.key, next));
                stack.push((&t.value, next));
            }
            Type::Record(t) => {
                stack.extend(t.fields.iter().map(|f| (&f.ty, next)));
                if let lash_kernel_value::RecordRest::Keep(rest) = &t.rest {
                    stack.push((rest, next));
                }
            }
            Type::Function(s) => {
                stack.push((&s.result, next));
                stack.extend(s.params.iter().map(|p| (&p.ty, next)));
            }
            Type::Any
            | Type::Json
            | Type::Null
            | Type::Absent
            | Type::Bool
            | Type::Int
            | Type::Float
            | Type::Number
            | Type::Text
            | Type::Bytes
            | Type::Timestamp
            | Type::Enum(_)
            | Type::Error
            | Type::Handle(_) => {}
        }
    }
    Ok(())
}
struct ByteCounter {
    used: usize,
    limit: usize,
}
impl std::io::Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.used = self
            .used
            .checked_add(bytes.len())
            .filter(|n| *n <= self.limit)
            .ok_or_else(|| std::io::Error::other("input byte budget"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Frame {
    site: Site,
    start: usize,
    child: u32,
}
/// A single append-only output buffer. Codecs emit final layout directly.
/// Sites are tracked while a map is collected or a caption waits to be
/// spelled; otherwise no frames, Sites, ranges or hashes are allocated.
pub struct SourceWriter {
    source: String,
    limit: usize,
    error: Option<PrintError>,
    frames: Option<Vec<Frame>>,
    sites: Option<Vec<SourceSite>>,
    /// The captions not yet spelled, by target.
    captions: BTreeMap<CaptionTarget, Caption>,
    #[cfg(test)]
    map_work: usize,
}
impl SourceWriter {
    pub fn new(options: PrintOptions) -> Self {
        Self {
            source: String::new(),
            limit: options.limits.max_bytes,
            error: None,
            frames: options.source_map.then(Vec::new),
            sites: options.source_map.then(Vec::new),
            captions: BTreeMap::new(),
            #[cfg(test)]
            map_work: 0,
        }
    }
    /// A writer for `document` that also holds `captions` for the codec to
    /// spell, checked by [`check_captions`] first.
    ///
    /// # Errors
    ///
    /// [`check_captions`]'s refusals.
    pub fn with_captions(
        options: PrintOptions,
        document: &Document,
        captions: Option<&Captions>,
    ) -> Result<Self, PrintError> {
        let mut writer = Self::new(options);
        if let Some(captions) = captions.filter(|captions| !captions.is_empty()) {
            check_captions(document, captions, options.limits)?;
            writer.captions = captions
                .entries()
                .iter()
                .map(|entry| (entry.target.clone(), entry.caption.clone()))
                .collect();
            writer.frames.get_or_insert_with(Vec::new);
        }
        Ok(writer)
    }
    /// Whether nodes' Sites are tracked: a codec opens its roots with
    /// [`Self::begin_node`] whenever they are.
    pub fn tracks_sites(&self) -> bool {
        self.frames.is_some()
    }
    /// The Site the next [`Self::begin_node`] (`None`) or
    /// [`Self::begin_child`] (`Some(index)`) opens, while Sites are tracked.
    pub fn next_site(&self, index: Option<u32>) -> Option<Site> {
        let parent = self.frames.as_ref()?.last()?;
        Some(parent.site.child(index.unwrap_or(parent.child)))
    }
    /// The caption of `target`, taken to be spelled: a caption never taken
    /// is reported dropped.
    pub fn take_caption(&mut self, target: impl Into<CaptionTarget>) -> Option<Caption> {
        self.captions.remove(&target.into())
    }
    /// Whether a caption not yet spelled names a target `wanted` picks, as
    /// a codec asks before it lays out what would spell it.
    pub fn holds_caption(&self, wanted: impl Fn(&CaptionTarget) -> bool) -> bool {
        self.captions.keys().any(wanted)
    }
    /// The bytes written so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.source.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.source.is_empty()
    }
    pub fn write(&mut self, text: &str) {
        if self.error.is_some() {
            return;
        }
        if self
            .source
            .len()
            .checked_add(text.len())
            .is_none_or(|size| size > self.limit)
        {
            self.error = Some(PrintError::Bytes { limit: self.limit });
        } else {
            self.source.push_str(text);
        }
    }
    /// Retain the first typed refusal; finish cannot report truncated success.
    pub fn refuse(&mut self, error: PrintError) {
        self.error.get_or_insert(error);
    }
    /// Serialize structured metadata in the dialect's native literal syntax.
    pub fn metadata(&mut self, value: &(impl serde::Serialize + ?Sized), format: SourceFormat) {
        let mut nested = Self::new(PrintOptions {
            limits: GeneratedSourceLimits {
                max_bytes: self.limit,
                ..Default::default()
            },
            ..Default::default()
        });
        nested.json(value);
        if let Some(error) = nested.error {
            self.refuse(error);
        } else {
            self.write(&crate::metadata::literal_tokens(
                &nested.source,
                format,
                false,
            ));
        }
    }
    pub fn json(&mut self, value: &(impl serde::Serialize + ?Sized)) {
        if let Err(error) = serde_json::to_writer(&mut *self, value) {
            self.error
                .get_or_insert_with(|| PrintError::Invalid(error.to_string()));
        }
    }
    pub fn begin_node(&mut self, root: Option<&Unit>) {
        if let Some(frames) = &mut self.frames {
            let site = if let Some(unit) = root {
                Site::new(unit.clone(), vec![])
            } else {
                let Some(parent) = frames.last_mut() else {
                    self.refuse(PrintError::Invalid("map child has no root".into()));
                    return;
                };
                let site = parent.site.child(parent.child);
                parent.child += 1;
                site
            };
            frames.push(Frame {
                site,
                start: self.source.len(),
                child: 0,
            });
            #[cfg(test)]
            {
                self.map_work += 1;
            }
        }
    }
    /// Begin a child by its document index when source syntax writes children
    /// in a different order from the document's tree traversal.
    pub fn begin_child(&mut self, index: u32) {
        if let Some(parent) = self.frames.as_mut().and_then(|frames| frames.last_mut()) {
            parent.child = index;
        }
        self.begin_node(None);
    }
    pub fn end_node(&mut self) {
        if let Some(frames) = &mut self.frames {
            let Some(frame) = frames.pop() else {
                self.refuse(PrintError::Invalid("unbalanced map spans".into()));
                return;
            };
            let end = self.source.len();
            let (ranges, implicit) = if end == frame.start {
                (vec![], Some(end))
            } else {
                (
                    vec![Span {
                        start: frame.start,
                        end,
                    }],
                    None,
                )
            };
            if let Some(sites) = &mut self.sites {
                sites.push(SourceSite {
                    site: frame.site,
                    ranges,
                    implicit,
                });
            }
        }
    }
    pub fn finish(
        self,
        document: &Document,
        format: SourceFormat,
    ) -> Result<PrintedSource, PrintError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let source_map = self
            .sites
            .map(|sites| -> Result<_, PrintError> {
                SourceMap::new(document, &self.source, format.language(), sites)
                    .map_err(|error| PrintError::Invalid(error.to_string()))
            })
            .transpose()?;
        Ok(PrintedSource {
            source: self.source,
            format,
            source_map,
            dropped: self.captions.into_keys().collect(),
        })
    }
}
impl std::io::Write for SourceWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let text = std::str::from_utf8(bytes).map_err(std::io::Error::other)?;
        self.write(text);
        if self.error.is_some() {
            return Err(std::io::Error::other("source byte budget"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    /// FIG-6373: printed maps are sorted even when child spans close first.
    #[test]
    fn printed_maps_are_canonical() {
        let document =
            lash_kernel_doc::parse_document("kernel 1\nnumbers float\nmain { return null }\n")
                .unwrap();
        let mut writer = SourceWriter::new(PrintOptions {
            source_map: true,
            ..Default::default()
        });
        writer.begin_node(Some(&Unit::Main));
        writer.write("{");
        writer.begin_node(None);
        writer.write("yield");
        writer.end_node();
        writer.write("}");
        writer.end_node();
        let printed = writer
            .finish(&document, SourceFormat::TypeScriptV1)
            .unwrap();
        let map = printed.source_map.unwrap();
        map.validate(&document, &printed.source).unwrap();
        assert!(map.sites.windows(2).all(|pair| pair[0].site < pair[1].site));
    }

    /// K-CAP-006: a codec spells the captions it takes, at Sites tracked
    /// with or without a map, and every caption it leaves is dropped.
    /// Caption bytes count against the byte budget before any is written.
    #[test]
    fn untaken_captions_are_dropped_and_caption_bytes_are_budgeted() {
        use lash_kernel_doc::{Caption, CaptionEntry, Captions};

        let document = lash_kernel_doc::parse_document(
            "kernel 1\nnumbers float\nmain { if true { print 1 } else { print 2 } return null }\n",
        )
        .unwrap();
        let entry = |path: &[u32], title: &str| {
            CaptionEntry::site(
                Site::new(Unit::Main, path.to_vec()),
                Caption::new(title, None).unwrap(),
            )
        };
        let captions = Captions::new(
            &document,
            vec![
                entry(&[0], "Decide"),
                entry(&[0, 2], "Otherwise"),
                entry(&[1], "Done"),
            ],
        )
        .unwrap();
        for source_map in [false, true] {
            let options = PrintOptions {
                source_map,
                ..Default::default()
            };
            let mut writer = SourceWriter::with_captions(options, &document, Some(&captions))
                .expect("valid captions");
            assert!(writer.tracks_sites());
            writer.begin_node(Some(&Unit::Main));
            let mut spelled = Vec::new();
            for _ in 0..2 {
                let site = writer.next_site(None).expect("tracked");
                if let Some(caption) = writer.take_caption(site) {
                    spelled.push(caption.title().to_owned());
                }
                writer.begin_node(None);
                writer.write("statement;");
                writer.end_node();
            }
            writer.end_node();
            let printed = writer
                .finish(&document, SourceFormat::TypeScriptV1)
                .unwrap();
            assert_eq!(spelled, ["Decide", "Done"]);
            assert_eq!(
                printed.dropped,
                [CaptionTarget::Site(Site::new(Unit::Main, vec![0, 2]))]
            );
            assert_eq!(printed.source_map.is_some(), source_map);
        }
        let tight = PrintOptions {
            limits: GeneratedSourceLimits {
                max_bytes: "DecideOtherwiseDone".len() - 1,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(matches!(
            SourceWriter::with_captions(tight, &document, Some(&captions)),
            Err(PrintError::Bytes { .. })
        ));
        let other = lash_kernel_doc::parse_document("kernel 1\nnumbers float\nmain { }\n").unwrap();
        assert!(matches!(
            SourceWriter::with_captions(PrintOptions::default(), &other, Some(&captions)),
            Err(PrintError::Invalid(_))
        ));
    }

    /// G-SOURCE-MAP-OFF: even root Site allocation is optional.
    #[test]
    fn disabled_collector_does_zero_map_work() {
        for enabled in [false, true] {
            let mut writer = SourceWriter::new(PrintOptions {
                source_map: enabled,
                ..Default::default()
            });
            writer.begin_node(Some(&Unit::Main));
            writer.write("{");
            writer.begin_node(None);
            writer.write("k.yield();");
            writer.end_node();
            writer.write("}");
            writer.end_node();
            assert_eq!(writer.map_work, if enabled { 2 } else { 0 });
        }
    }
}
