//! Small session descriptors. Bindings, functions, notes and heap are
//! page roots: the descriptor a session publishes never grows with what
//! the session holds (FIG-6418).
use lash_kernel_doc::graph_tokens::{TokenCodec, TokenReader, TokenWriter, encode_body};
use lash_kernel_doc::heap::{HeapFault, PageRef};
use std::collections::BTreeSet;

use lash_kernel_doc::{FunctionId, NumberPolicy};
use lash_kernel_state::paged::tree::grant;
use lash_kernel_state::paged::{KeyedRoot, PageBody, PageKind, PageSource, SessionRoot};

pub(super) struct SessionDescriptor {
    pub dialect: String,
    pub numbers: NumberPolicy,
    pub kernel: u32,
    pub cells: u64,
    pub session: SessionRoot,
    /// Each name's session notes, keyed by name beside the bindings
    /// (notes.rs).
    pub notes: KeyedRoot,
    /// The library functions the session's saved functions pin, together
    /// (FIG-5799): bounded by the build's library, never by the session.
    pub pins: BTreeSet<FunctionId>,
}
impl TokenCodec for SessionDescriptor {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(7);
        self.dialect.encode(out);
        self.numbers.encode(out);
        self.kernel.encode(out);
        self.cells.encode(out);
        self.session.encode(out);
        self.notes.encode(out);
        self.pins.encode(out);
    }
    fn decode(
        input: &mut TokenReader<'_>,
    ) -> Result<Self, lash_kernel_doc::graph_tokens::TokenError> {
        input.expect_struct(7)?;
        Ok(Self {
            dialect: String::decode(input)?,
            numbers: NumberPolicy::decode(input)?,
            kernel: u32::decode(input)?,
            cells: u64::decode(input)?,
            session: SessionRoot::decode(input)?,
            notes: KeyedRoot::decode(input)?,
            pins: BTreeSet::decode(input)?,
        })
    }
}
pub(super) fn seal(
    source: &mut dyn PageSource,
    descriptor: &SessionDescriptor,
) -> Result<PageRef, HeapFault> {
    let mut children = vec![descriptor.session.heap];
    children.extend(descriptor.session.bindings.page());
    children.extend(descriptor.session.functions.page());
    children.extend(descriptor.notes.page());
    children.sort_by_key(|p| p.id);
    children.dedup_by_key(|p| p.id);
    source.seal(PageBody {
        kind: PageKind::SESSION_ROOT,
        tokens: encode_body(descriptor),
        children,
    })
}
pub(super) fn read(
    source: &mut dyn PageSource,
    page: PageRef,
) -> Result<SessionDescriptor, HeapFault> {
    let answers = source.read(&[page])?;
    let [answer] = answers.as_slice() else {
        return Err(HeapFault::Malformed(
            "session port answered wrong count".into(),
        ));
    };
    if answer.page.id != page.id || answer.kind != PageKind::SESSION_ROOT {
        return Err(HeapFault::Corrupt {
            page: page.id,
            reason: "expected paged session root".into(),
        });
    }
    let mut input = TokenReader::new(&answer.tokens);
    let mut descriptor = SessionDescriptor::decode(&mut input).map_err(|e| HeapFault::Corrupt {
        page: page.id,
        reason: e.to_string(),
    })?;
    input.finish().map_err(|e| HeapFault::Corrupt {
        page: page.id,
        reason: e.to_string(),
    })?;
    grant(&mut descriptor.session.heap, &answer.children)?;
    if let Some(page) = descriptor.session.bindings.page_mut() {
        grant(page, &answer.children)?;
    }
    if let Some(page) = descriptor.session.functions.page_mut() {
        grant(page, &answer.children)?;
    }
    if let Some(page) = descriptor.notes.page_mut() {
        grant(page, &answer.children)?;
    }
    Ok(descriptor)
}
