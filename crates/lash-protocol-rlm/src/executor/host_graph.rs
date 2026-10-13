//! A cell host's state as checkpoint graph nodes (FIG-5887 §3).
//!
//! What a cell's host holds across parks is three ordered sequences and a
//! few facts:
//!
//! - the **call summary**: one [`CallSummary`] per call the cell admitted,
//!   by admission ordinal, naming its Admission (kind 18) and, once it
//!   settled, its one CallOutcome (kind 20). It never holds a call's
//!   request or its record: those are the admission's and the outcome's.
//! - the cell's **prints** and the **processes it started**, as immutable
//!   event items.
//! - the `max_tool_calls` refusal it met, its one control attempt, and the
//!   call whose cancellation ends the cell.
//!
//! Each sequence is a dense radix tree on `ordinal / 32` ([`Paged`]):
//! pages of up to 32 items (SummaryPage 22, EventPage 25), and indexes of
//! up to 32 children (SummaryIndex 23, EventIndex 26), bottom up, the top
//! level's single node its root. Appending rewrites the last page and its
//! index path; settling a call, in whatever order calls complete, rewrites
//! only the page that holds it and that path. Every other page keeps its
//! node, so a park installs the pages it changed and nothing else. A tree
//! whose shape is not the one canonical shape for its count is refused, and
//! so is a hole or a count that disagrees with its items.
//!
//! [`HostRoot`] (kind 24) is the root of it all, at most 4 KiB: the counts
//! and roots of the three trees and the facts.

use std::collections::BTreeSet;

use lash_core::{
    ProcessId, ToolCallId, ToolCallLimitExceeded, ToolCallLimitScope, TurnControlKind,
};
use lash_core_store::checkpoint_graph::{
    GraphDecoder, GraphRefusal, GraphStaging, NodeId, NodeKind, TokenCodec, TokenError,
    TokenReader, TokenWriter,
};
use lash_kernel_doc::{Datum, EffectIdentity, EffectName};

/// Items per page and children per index.
pub const FANOUT: u64 = 32;

fn unsupported(variant: u32) -> TokenError {
    TokenError::UnsupportedVariant { variant }
}

fn invalid(message: impl Into<String>) -> TokenError {
    TokenError::Invalid(message.into())
}

fn shape(kind: NodeKind, reason: &str) -> GraphRefusal {
    GraphRefusal::InvalidBody {
        kind: kind.0,
        reason: reason.to_owned(),
    }
}

/// What a paged sequence holds, and the kinds its nodes take.
pub trait PagedItem: TokenCodec + Clone {
    const PAGE: NodeKind;
    const INDEX: NodeKind;
    /// The event stream an event page and index name first: Print 0,
    /// Process 1. `None` for the call summary.
    const STREAM: Option<u32>;

    /// Refuses an item that is not the one at `ordinal`.
    fn check_at(&self, _ordinal: u64) -> Result<(), GraphRefusal> {
        Ok(())
    }
}

/// A dense ordered sequence, by ordinal from 0, with each page's node kept
/// until an item on it changes.
#[derive(Clone, Debug, PartialEq)]
pub struct Paged<T> {
    items: Vec<T>,
    /// The node of each page staged since it last changed.
    pages: Vec<Option<NodeId>>,
}

impl<T> Default for Paged<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            pages: Vec::new(),
        }
    }
}

fn page_of(ordinal: u64) -> usize {
    usize::try_from(ordinal / FANOUT).unwrap_or(usize::MAX)
}

/// How many items a node at `level` (a page is level 0) holds when full.
fn capacity(level: u64) -> u64 {
    u32::try_from(level + 1)
        .ok()
        .and_then(|exponent| FANOUT.checked_pow(exponent))
        .unwrap_or(u64::MAX)
}

/// An index's body: what it covers and its children.
struct IndexBody {
    stream: Option<u32>,
    level: u64,
    first: u64,
    count: u64,
    children: Vec<(NodeId, u64)>,
}

impl IndexBody {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(4 + u64::from(self.stream.is_some()));
        if let Some(stream) = self.stream {
            out.unsigned(u64::from(stream));
        }
        out.unsigned(self.level);
        out.unsigned(self.first);
        out.unsigned(self.count);
        out.seq(self.children.len() as u64);
        for (child, count) in &self.children {
            out.seq(2);
            child.encode(out);
            out.unsigned(*count);
        }
    }

    fn decode(input: &mut TokenReader<'_>, stream: Option<u32>) -> Result<Self, TokenError> {
        input.expect_struct(4 + u64::from(stream.is_some()))?;
        let stream = match stream {
            Some(expected) => {
                let found = input.unsigned_u32()?;
                if found != expected {
                    return Err(invalid("an event index of another stream"));
                }
                Some(found)
            }
            None => None,
        };
        let level = input.unsigned()?;
        let first = input.unsigned()?;
        let count = input.unsigned()?;
        let children_count = input.seq()?;
        if children_count > FANOUT {
            return Err(invalid("an index holds at most 32 children"));
        }
        let mut children = Vec::new();
        for _ in 0..children_count {
            input.expect_struct(2)?;
            children.push((NodeId::decode(input)?, input.unsigned()?));
        }
        Ok(Self {
            stream,
            level,
            first,
            count,
            children,
        })
    }
}

/// A page's body: its first ordinal and its items.
struct PageBody<T> {
    first: u64,
    items: Vec<T>,
}

impl<T: PagedItem> TokenCodec for PageBody<T> {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2 + u64::from(T::STREAM.is_some()));
        if let Some(stream) = T::STREAM {
            out.unsigned(u64::from(stream));
        }
        out.unsigned(self.first);
        self.items.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2 + u64::from(T::STREAM.is_some()))?;
        if let Some(expected) = T::STREAM
            && input.unsigned_u32()? != expected
        {
            return Err(invalid("an event page of another stream"));
        }
        let first = input.unsigned()?;
        let items = Vec::decode(input)?;
        if items.len() as u64 > FANOUT {
            return Err(invalid("a page holds at most 32 items"));
        }
        Ok(Self { first, items })
    }
}

struct IndexNode<'a>(&'a IndexBody);

impl TokenCodec for IndexNode<'_> {
    fn encode(&self, out: &mut TokenWriter) {
        self.0.encode(out);
    }

    fn decode(_input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Err(invalid("an index is read through its stream"))
    }
}

impl<T: PagedItem> Paged<T> {
    #[must_use]
    pub fn len(&self) -> u64 {
        self.items.len() as u64
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[must_use]
    pub fn items(&self) -> &[T] {
        &self.items
    }

    #[must_use]
    pub fn get(&self, ordinal: u64) -> Option<&T> {
        self.items.get(usize::try_from(ordinal).ok()?)
    }

    fn touch(&mut self, ordinal: u64) {
        let page = page_of(ordinal);
        if self.pages.len() <= page {
            self.pages.resize(page + 1, None);
        }
        self.pages[page] = None;
    }

    /// Appends `item` and returns its ordinal.
    pub fn push(&mut self, item: T) -> u64 {
        let ordinal = self.len();
        self.items.push(item);
        self.touch(ordinal);
        ordinal
    }

    /// Replaces the item at `ordinal`; `false` when there is none.
    pub fn replace(&mut self, ordinal: u64, item: T) -> bool {
        let Some(slot) = usize::try_from(ordinal)
            .ok()
            .and_then(|at| self.items.get_mut(at))
        else {
            return false;
        };
        *slot = item;
        self.touch(ordinal);
        true
    }

    /// Forgets every staged page: the commit that would have installed
    /// them was refused, so the next stage writes them again.
    pub fn unstaged(&mut self) {
        self.pages.iter_mut().for_each(|page| *page = None);
    }

    /// Stages every page that changed since it was last staged, and the
    /// index path above them; returns the root, or `None` when empty. A
    /// page staged earlier and unchanged since is not written again: it is
    /// added to `reads`, the existing nodes the commit requires.
    ///
    /// # Errors
    ///
    /// Any refusal of the staging area.
    pub fn stage(
        &mut self,
        staging: &mut GraphStaging,
        reads: &mut BTreeSet<NodeId>,
    ) -> Result<Option<NodeId>, GraphRefusal> {
        let mut level: Vec<(NodeId, u64, u64)> = Vec::new();
        for (page, chunk) in self.items.chunks(FANOUT as usize).enumerate() {
            let first = page as u64 * FANOUT;
            let cached = self.pages.get(page).copied().flatten();
            let node = match cached {
                Some(node) => {
                    if staging.get(&node).is_none() {
                        reads.insert(node);
                    }
                    node
                }
                None => {
                    let node = staging.encode_value(
                        T::PAGE,
                        &PageBody {
                            first,
                            items: chunk.to_vec(),
                        },
                    )?;
                    if self.pages.len() <= page {
                        self.pages.resize(page + 1, None);
                    }
                    self.pages[page] = Some(node);
                    node
                }
            };
            level.push((node, first, chunk.len() as u64));
        }
        let mut height = 1;
        while level.len() > 1 {
            let mut next = Vec::new();
            for group in level.chunks(FANOUT as usize) {
                let first = group[0].1;
                let count = group.iter().map(|(_, _, count)| count).sum();
                let body = IndexBody {
                    stream: T::STREAM,
                    level: height,
                    first,
                    count,
                    children: group
                        .iter()
                        .map(|(node, _, count)| (*node, *count))
                        .collect(),
                };
                let node = staging.encode_value(T::INDEX, &IndexNode(&body))?;
                next.push((node, first, count));
            }
            level = next;
            height += 1;
        }
        Ok(level.first().map(|(node, _, _)| *node))
    }

    /// Reads the sequence of `count` items rooted at `root`, refusing any
    /// shape but the canonical one, a hole, or a count its nodes disagree
    /// with. `parent` is the node that references the root.
    ///
    /// # Errors
    ///
    /// Any refusal of a node read, [`GraphRefusal::WrongChildKind`], or
    /// [`GraphRefusal::InvalidBody`] for a non-canonical tree.
    pub fn load(
        decoder: &mut GraphDecoder<'_>,
        parent: NodeId,
        root: Option<NodeId>,
        count: u64,
    ) -> Result<Self, GraphRefusal> {
        let mut paged = Self::default();
        match (root, count) {
            (None, 0) => return Ok(paged),
            (Some(root), count) if count > 0 => {
                let kind = decoder.child_kind(parent, root, &[T::PAGE, T::INDEX])?;
                // The root is at the lowest level that holds `count`.
                let mut level = 0;
                while capacity(level) < count {
                    level += 1;
                }
                if (kind == T::PAGE) != (level == 0) {
                    return Err(shape(kind, "a tree's root is not at its lowest level"));
                }
                paged.read(decoder, root, level, 0, count)?;
            }
            _ => return Err(shape(T::PAGE, "a tree's count disagrees with its root")),
        }
        paged.pages = vec![None; paged.items.len().div_ceil(FANOUT as usize)];
        Ok(paged)
    }

    fn read(
        &mut self,
        decoder: &mut GraphDecoder<'_>,
        id: NodeId,
        level: u64,
        first: u64,
        count: u64,
    ) -> Result<(), GraphRefusal> {
        if level == 0 {
            let page: PageBody<T> = decoder.decode(id, T::PAGE)?;
            if page.first != first || page.items.len() as u64 != count || count == 0 {
                return Err(shape(T::PAGE, "a page is not the one its index names"));
            }
            for (offset, item) in page.items.into_iter().enumerate() {
                item.check_at(first + offset as u64)?;
                self.items.push(item);
            }
            return Ok(());
        }
        let logical = decoder.logical_body(id, T::INDEX)?;
        let refuse = |error| GraphRefusal::from_token(error, Some(id), Some(T::INDEX.0));
        let mut reader = TokenReader::new(&logical);
        let index = IndexBody::decode(&mut reader, T::STREAM).map_err(refuse)?;
        reader.finish().map_err(refuse)?;
        let full = capacity(level - 1);
        let last = index.children.len().saturating_sub(1);
        let sums = index.children.iter().map(|(_, count)| count).sum::<u64>();
        if index.level != level
            || index.first != first
            || index.count != count
            || sums != count
            || index.children.is_empty()
            || index
                .children
                .iter()
                .enumerate()
                .any(|(at, (_, count))| *count == 0 || (at < last && *count != full))
        {
            return Err(shape(
                T::INDEX,
                "an index is not the one its tree's count makes",
            ));
        }
        let child_kind = if level == 1 { T::PAGE } else { T::INDEX };
        let mut next = first;
        for (child, child_count) in index.children {
            decoder.child_kind(id, child, &[child_kind])?;
            self.read(decoder, child, level - 1, next, child_count)?;
            next += child_count;
        }
        Ok(())
    }
}

// --- The call summary ----------------------------------------------------

/// How a summarized call stands: pending its outcome, or completed with
/// its one outcome node. The bits are checked projections of the outcome,
/// never the truth of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallStatus {
    /// 0.
    Pending,
    /// 1.
    Completed {
        outcome: NodeId,
        success: bool,
        cancelled: bool,
        control: Option<TurnControlKind>,
    },
}

/// One call the cell admitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallSummary {
    pub ordinal: u64,
    /// Where the cell performed it.
    pub effect: EffectIdentity,
    /// The effect, as the cell's source names it.
    pub operation: EffectName,
    pub call_id: ToolCallId,
    /// Its Admission, which holds its request.
    pub admission: NodeId,
    pub status: CallStatus,
}

fn encode_control(control: Option<TurnControlKind>, out: &mut TokenWriter) {
    match control {
        None => out.none(),
        Some(kind) => {
            out.some();
            out.sum(
                match kind {
                    TurnControlKind::Finish => 0,
                    TurnControlKind::SwitchAgentFrame => 1,
                },
                0,
            );
        }
    }
}

fn decode_control(input: &mut TokenReader<'_>) -> Result<Option<TurnControlKind>, TokenError> {
    if !input.option()? {
        return Ok(None);
    }
    let (variant, fields) = input.sum()?;
    TokenReader::expect_fields(fields, 0)?;
    match variant {
        0 => Ok(Some(TurnControlKind::Finish)),
        1 => Ok(Some(TurnControlKind::SwitchAgentFrame)),
        other => Err(unsupported(other)),
    }
}

impl TokenCodec for CallSummary {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(6);
        out.unsigned(self.ordinal);
        self.effect.encode(out);
        self.operation.encode(out);
        out.text(self.call_id.as_str());
        self.admission.encode(out);
        match &self.status {
            CallStatus::Pending => out.sum(0, 0),
            CallStatus::Completed {
                outcome,
                success,
                cancelled,
                control,
            } => {
                out.sum(1, 4);
                outcome.encode(out);
                out.bool(*success);
                out.bool(*cancelled);
                encode_control(*control, out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(6)?;
        let ordinal = input.unsigned()?;
        let effect = EffectIdentity::decode(input)?;
        let operation = EffectName::decode(input)?;
        let call_id =
            ToolCallId::parse(input.text()?).map_err(|error| invalid(error.to_string()))?;
        let admission = NodeId::decode(input)?;
        let (variant, fields) = input.sum()?;
        let status = match variant {
            0 => {
                TokenReader::expect_fields(fields, 0)?;
                CallStatus::Pending
            }
            1 => {
                TokenReader::expect_fields(fields, 4)?;
                CallStatus::Completed {
                    outcome: NodeId::decode(input)?,
                    success: input.bool()?,
                    cancelled: input.bool()?,
                    control: decode_control(input)?,
                }
            }
            other => return Err(unsupported(other)),
        };
        Ok(Self {
            ordinal,
            effect,
            operation,
            call_id,
            admission,
            status,
        })
    }
}

impl PagedItem for CallSummary {
    const PAGE: NodeKind = NodeKind::SUMMARY_PAGE;
    const INDEX: NodeKind = NodeKind::SUMMARY_INDEX;
    const STREAM: Option<u32> = None;

    fn check_at(&self, ordinal: u64) -> Result<(), GraphRefusal> {
        if self.ordinal == ordinal {
            Ok(())
        } else {
            Err(shape(Self::PAGE, "a summary is not at its own ordinal"))
        }
    }
}

impl CallSummary {
    /// The status bits `output` projects: what a completed call's summary
    /// must say of its outcome.
    pub fn status_of(outcome: NodeId, output: &lash_core::ToolCallOutput) -> CallStatus {
        CallStatus::Completed {
            outcome,
            success: output.is_success(),
            cancelled: matches!(output.outcome, lash_core::ToolCallOutcome::Cancelled(_)),
            control: output.turn_control_kind(),
        }
    }

    /// Checks the summary's references and status bits against the nodes
    /// they name: its Admission, and its outcome, whose output the bits
    /// must project exactly and whose call must be this one.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::WrongChildKind`], [`GraphRefusal::InvalidBody`] for
    /// bits or a call the outcome does not hold, or any refusal of a node
    /// read.
    pub fn check(
        &self,
        decoder: &mut GraphDecoder<'_>,
        page: NodeId,
    ) -> Result<Option<lash_vm_broker::round_graph::CallOutcome>, GraphRefusal> {
        decoder.child_kind(page, self.admission, &[NodeKind::ADMISSION])?;
        let CallStatus::Completed { outcome, .. } = &self.status else {
            return Ok(None);
        };
        let loaded = lash_vm_broker::round_graph::CallOutcome::load(decoder, *outcome)?;
        if Self::status_of(*outcome, &loaded.output) != self.status
            || loaded.call_id != self.call_id
            || loaded.admission != Some(self.admission)
        {
            return Err(shape(
                NodeKind::SUMMARY_PAGE,
                "a summary's bits are not its outcome's",
            ));
        }
        Ok(Some(loaded))
    }
}

// --- Events --------------------------------------------------------------

/// One print, as the cell printed it.
#[derive(Clone, Debug, PartialEq)]
pub struct PrintEvent(pub Datum);

impl TokenCodec for PrintEvent {
    fn encode(&self, out: &mut TokenWriter) {
        self.0.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Datum::decode(input).map(Self)
    }
}

impl PagedItem for PrintEvent {
    const PAGE: NodeKind = NodeKind::EVENT_PAGE;
    const INDEX: NodeKind = NodeKind::EVENT_INDEX;
    const STREAM: Option<u32> = Some(0);
}

/// One process the cell started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessEvent(pub ProcessId);

impl TokenCodec for ProcessEvent {
    fn encode(&self, out: &mut TokenWriter) {
        out.text(self.0.as_str());
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        ProcessId::parse(input.text()?)
            .map(Self)
            .map_err(|error| invalid(error.to_string()))
    }
}

impl PagedItem for ProcessEvent {
    const PAGE: NodeKind = NodeKind::EVENT_PAGE;
    const INDEX: NodeKind = NodeKind::EVENT_INDEX;
    const STREAM: Option<u32> = Some(1);
}

// --- The host root -------------------------------------------------------

/// Kind 24: a cell host's state at a park.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HostState {
    pub calls: Paged<CallSummary>,
    /// The `max_tool_calls` refusal the cell met, if it met one.
    pub tool_call_limit: Option<ToolCallLimitExceeded>,
    /// The ordinal of the cell's one admitted control call.
    pub control_attempt: Option<u64>,
    /// The ordinal of the first call that settled cancelled: what ends the
    /// cell whatever its program made of the error.
    pub cancelled: Option<u64>,
    pub prints: Paged<PrintEvent>,
    pub processes: Paged<ProcessEvent>,
    /// How many reads the cell made up to this state (ADR 0144): they count
    /// against `max_tool_calls` beside its calls. A read made after the
    /// state was saved is made again after a crash, and counted again.
    pub reads: u64,
}

/// The host root's body: counts, roots and facts.
struct HostRootBody {
    calls: (u64, Option<NodeId>),
    tool_call_limit: Option<ToolCallLimitExceeded>,
    control_attempt: Option<u64>,
    cancelled: Option<u64>,
    prints: (u64, Option<NodeId>),
    processes: (u64, Option<NodeId>),
    reads: u64,
}

fn encode_tree_ref(tree: (u64, Option<NodeId>), out: &mut TokenWriter) {
    out.seq(2);
    out.unsigned(tree.0);
    tree.1.encode(out);
}

fn decode_tree_ref(input: &mut TokenReader<'_>) -> Result<(u64, Option<NodeId>), TokenError> {
    input.expect_struct(2)?;
    Ok((input.unsigned()?, Option::decode(input)?))
}

fn usize_token(value: usize) -> u64 {
    value as u64
}

impl TokenCodec for HostRootBody {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(7);
        encode_tree_ref(self.calls, out);
        match &self.tool_call_limit {
            None => out.none(),
            Some(exceeded) => {
                out.some();
                out.seq(4);
                out.sum(
                    match exceeded.scope {
                        ToolCallLimitScope::Cell => 0,
                        ToolCallLimitScope::Process => 1,
                    },
                    0,
                );
                out.unsigned(usize_token(exceeded.limit.get()));
                out.unsigned(usize_token(exceeded.counted));
                out.unsigned(usize_token(exceeded.requested));
            }
        }
        self.control_attempt.encode(out);
        self.cancelled.encode(out);
        encode_tree_ref(self.prints, out);
        encode_tree_ref(self.processes, out);
        out.unsigned(self.reads);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(7)?;
        let calls = decode_tree_ref(input)?;
        let tool_call_limit = if input.option()? {
            input.expect_struct(4)?;
            let (scope, fields) = input.sum()?;
            TokenReader::expect_fields(fields, 0)?;
            let scope = match scope {
                0 => ToolCallLimitScope::Cell,
                1 => ToolCallLimitScope::Process,
                other => return Err(unsupported(other)),
            };
            let width = |value: u64| usize::try_from(value).map_err(|_| TokenError::LengthOverflow);
            let limit = std::num::NonZeroUsize::new(width(input.unsigned()?)?)
                .ok_or_else(|| invalid("a tool-call limit is at least one"))?;
            Some(ToolCallLimitExceeded {
                scope,
                limit: limit.into(),
                counted: width(input.unsigned()?)?,
                requested: width(input.unsigned()?)?,
            })
        } else {
            None
        };
        Ok(Self {
            calls,
            tool_call_limit,
            control_attempt: Option::decode(input)?,
            cancelled: Option::decode(input)?,
            prints: decode_tree_ref(input)?,
            processes: decode_tree_ref(input)?,
            reads: input.unsigned()?,
        })
    }
}

impl HostState {
    /// Forgets every staged page: the commit was refused.
    pub fn unstaged(&mut self) {
        self.calls.unstaged();
        self.prints.unstaged();
        self.processes.unstaged();
    }

    /// Stages the pages that changed and the root; returns the HostRoot,
    /// adding the unchanged pages it reuses to `reads`.
    ///
    /// # Errors
    ///
    /// Any refusal of the staging area, [`GraphRefusal::RootTooLarge`]
    /// included.
    pub fn stage(
        &mut self,
        staging: &mut GraphStaging,
        reads: &mut BTreeSet<NodeId>,
    ) -> Result<NodeId, GraphRefusal> {
        let body = HostRootBody {
            calls: (self.calls.len(), self.calls.stage(staging, reads)?),
            tool_call_limit: self.tool_call_limit,
            control_attempt: self.control_attempt,
            cancelled: self.cancelled,
            prints: (self.prints.len(), self.prints.stage(staging, reads)?),
            processes: (self.processes.len(), self.processes.stage(staging, reads)?),
            reads: self.reads,
        };
        staging.encode_value(NodeKind::HOST_ROOT, &body)
    }

    /// Reads HostRoot `id` back whole, checking every tree's shape, every
    /// summary against its admission and outcome, and that the facts name
    /// calls the summary holds: the control attempt a call that exists, and
    /// the cancellation the first call whose outcome is cancelled.
    ///
    /// # Errors
    ///
    /// Any refusal of a node read or a check.
    pub fn load(decoder: &mut GraphDecoder<'_>, id: NodeId) -> Result<Self, GraphRefusal> {
        let body: HostRootBody = decoder.decode(id, NodeKind::HOST_ROOT)?;
        let calls: Paged<CallSummary> = Paged::load(decoder, id, body.calls.1, body.calls.0)?;
        for call in calls.items() {
            call.check(decoder, id)?;
        }
        let first_cancelled = calls
            .items()
            .iter()
            .find(|call| {
                matches!(
                    call.status,
                    CallStatus::Completed {
                        cancelled: true,
                        ..
                    }
                )
            })
            .map(|call| call.ordinal);
        if first_cancelled != body.cancelled
            || body
                .control_attempt
                .is_some_and(|ordinal| ordinal >= calls.len())
        {
            return Err(shape(
                NodeKind::HOST_ROOT,
                "a host fact names no call the summary holds",
            ));
        }
        Ok(Self {
            calls,
            tool_call_limit: body.tool_call_limit,
            control_attempt: body.control_attempt,
            cancelled: body.cancelled,
            prints: Paged::load(decoder, id, body.prints.1, body.prints.0)?,
            processes: Paged::load(decoder, id, body.processes.1, body.processes.0)?,
            reads: body.reads,
        })
    }

    /// Records the admission of a call: its summary, pending, at the next
    /// ordinal, and whether it is the cell's control call.
    pub fn admit(
        &mut self,
        effect: EffectIdentity,
        operation: EffectName,
        call_id: ToolCallId,
        admission: NodeId,
        control: bool,
    ) -> u64 {
        let ordinal = self.calls.len();
        self.calls.push(CallSummary {
            ordinal,
            effect,
            operation,
            call_id,
            admission,
            status: CallStatus::Pending,
        });
        if control && self.control_attempt.is_none() {
            self.control_attempt = Some(ordinal);
        }
        ordinal
    }

    /// Records the settlement of `call_id` as `outcome` with `output`, in
    /// whatever order calls settle. `false` when the summary holds no such
    /// pending call: a settled call is never settled again.
    pub fn settle(
        &mut self,
        call_id: &ToolCallId,
        outcome: NodeId,
        output: &lash_core::ToolCallOutput,
    ) -> bool {
        let Some(summary) = self
            .calls
            .items()
            .iter()
            .find(|call| &call.call_id == call_id && call.status == CallStatus::Pending)
            .cloned()
        else {
            return false;
        };
        let status = CallSummary::status_of(outcome, output);
        if matches!(
            status,
            CallStatus::Completed {
                cancelled: true,
                ..
            }
        ) && self.cancelled.is_none_or(|first| summary.ordinal < first)
        {
            self.cancelled = Some(summary.ordinal);
        }
        let ordinal = summary.ordinal;
        self.calls
            .replace(ordinal, CallSummary { status, ..summary })
    }
}

pub use lash_vm_broker::kernel::graph::CellEnd;

#[cfg(test)]
#[path = "host_graph_tests.rs"]
mod tests;
