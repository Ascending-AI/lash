//! Small session descriptors. Bindings, functions and heap are page roots.
use lash_kernel_doc::graph_tokens::{TokenCodec, TokenReader, TokenWriter, encode_body};
use lash_kernel_doc::heap::{HeapFault, PageRef};
use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{FunctionId, Name, NumberPolicy};
use lash_kernel_state::paged::tree::grant;
use lash_kernel_state::paged::{PageBody, PageKind, PageSource, SessionRoot};

pub(super) struct SessionDescriptor {
    pub dialect: String,
    pub numbers: NumberPolicy,
    pub kernel: u32,
    pub cells: u64,
    pub session: SessionRoot,
    /// Every name the session binds: what a cell is lowered against and a
    /// tool's namespace is checked against, read without the bindings.
    pub names: BTreeSet<Name>,
    /// Each name an earlier cell bound that the session does not carry,
    /// with why (`K-SES-003`), until a later cell binds it again.
    pub not_carried: BTreeMap<Name, String>,
    /// Each saved function's declared name, with the cell whose end froze
    /// what it reads and the library functions its document pins
    /// (FIG-5799).
    pub functions: BTreeMap<Name, (u64, BTreeSet<FunctionId>)>,
}
impl TokenCodec for SessionDescriptor {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(8);
        self.dialect.encode(out);
        self.numbers.encode(out);
        self.kernel.encode(out);
        self.cells.encode(out);
        self.session.encode(out);
        self.names.encode(out);
        self.not_carried.encode(out);
        self.functions.encode(out);
    }
    fn decode(
        input: &mut TokenReader<'_>,
    ) -> Result<Self, lash_kernel_doc::graph_tokens::TokenError> {
        input.expect_struct(8)?;
        Ok(Self {
            dialect: String::decode(input)?,
            numbers: NumberPolicy::decode(input)?,
            kernel: u32::decode(input)?,
            cells: u64::decode(input)?,
            session: SessionRoot::decode(input)?,
            names: BTreeSet::decode(input)?,
            not_carried: BTreeMap::decode(input)?,
            functions: BTreeMap::decode(input)?,
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
    Ok(descriptor)
}
