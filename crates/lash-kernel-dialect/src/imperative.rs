//! Bounded final-text writing and optional Site collection for generated codecs.
use lash_kernel_doc::{Document, DocumentId, Node, Site, Unit};
use sha2::{Digest, Sha256};

/// Generated source is a separate, disposable format from stored documents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFormat {
    TypeScriptV1,
    PythonV1,
}

impl SourceFormat {
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
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SourceMap {
    /// SHA-256 of the final UTF-8 source bytes (without a domain prefix).
    pub source: [u8; 32],
    pub document: DocumentId,
    pub format: SourceFormat,
    pub sites: Vec<SourceSite>,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SourceSite {
    pub site: Site,
    pub ranges: Vec<crate::Span>,
    /// Reserved for genuinely unspelled blocks. Generic forms spell all blocks.
    pub implicit: Option<usize>,
}
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PrintError {
    #[error("generated source exceeds byte budget {limit}")]
    Bytes { limit: usize },
    #[error("generated source exceeds depth budget {limit}")]
    Depth { limit: usize },
    #[error("cannot encode generated source: {0}")]
    Encode(#[from] lash_kernel_doc::EncodeError),
    #[error("invalid generated source: {0}")]
    Invalid(String),
}
impl From<PrintError> for crate::Diagnostic {
    fn from(error: PrintError) -> Self {
        Self {
            code: match &error {
                PrintError::Bytes { .. } => "GENERATED_SOURCE_BYTES",
                PrintError::Depth { .. } => "GENERATED_SOURCE_DEPTH",
                PrintError::Encode(_) | PrintError::Invalid(_) => "GENERATED_SOURCE_INVALID",
            }
            .into(),
            message: error.to_string(),
            span: None,
            kind: crate::DiagnosticKind::Refusal,
            repairs: vec![],
        }
    }
}

/// Refuse depth before any recursive conversion, rendering, hashing or encoding.
/// The iterative walk is ordinary structural work, also required without maps.
pub fn check_document_depth(
    document: &Document,
    limits: GeneratedSourceLimits,
) -> Result<(), PrintError> {
    let limit = limits.max_depth.min(64);
    for effect in document.manifest.effects.keys() {
        check_leaf_size(effect.as_str().len(), limits.max_bytes)?;
    }
    for label in document.manifest.functions.values() {
        check_leaf_size(label.as_str().len(), limits.max_bytes)?;
    }
    let roots = std::iter::once(Node::Block(&document.main))
        .chain(document.functions.values().map(|f| Node::Block(&f.body)));
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
    for signature in document
        .manifest
        .effects
        .values()
        .chain(document.entries.values())
    {
        check_signature(signature, limit)?;
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
        Action::Perform { effect, args, .. } => {
            check_leaf_size(effect.as_str().len(), limit)?;
            for atom in args {
                check(atom)?;
            }
        }
        Action::Sleep { duration } => check(duration)?,
        Action::Join { task } | Action::Cancel { task } => check(task)?,
        Action::JoinMany { tasks, .. } => check(tasks)?,
        Action::Yield => {}
    }
    Ok(())
}
fn check_signature(signature: &lash_kernel_doc::Signature, limit: usize) -> Result<(), PrintError> {
    check_type(&signature.result, limit)?;
    for param in &signature.params {
        check_type(&param.ty, limit)?;
    }
    Ok(())
}
fn check_type(ty: &lash_kernel_doc::Type, limit: usize) -> Result<(), PrintError> {
    use lash_kernel_doc::Type;
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
                if let Some(rest) = &t.rest {
                    stack.push((rest, next));
                }
            }
            Type::Function(s) => {
                stack.push((&s.result, next));
                stack.extend(s.params.iter().map(|p| (&p.ty, next)));
            }
            Type::Any
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
/// A disabled collector allocates no frames, Sites, ranges or hashes.
pub struct SourceWriter {
    source: String,
    limit: usize,
    error: Option<PrintError>,
    frames: Option<Vec<Frame>>,
    sites: Option<Vec<SourceSite>>,
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
            #[cfg(test)]
            map_work: 0,
        }
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
                    vec![crate::Span {
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
            } else {
                self.refuse(PrintError::Invalid("missing map collector".into()));
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
                Ok(SourceMap {
                    source: Sha256::digest(self.source.as_bytes()).into(),
                    document: document.identity()?,
                    format,
                    sites,
                })
            })
            .transpose()?;
        Ok(PrintedSource {
            source: self.source,
            format,
            source_map,
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
