//! One body execution's tracing state: its open extension spans and their
//! holds, its budget, its omissions, and the mutex that linearizes every
//! end against the revocation at the body's exit (§7.4 rule 4).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use lash_sansio::sync::MutexExt as _;
use lash_trace::{
    ExtensionAttribute, ExtensionContent, ExtensionOperation, ExtensionSpanEnd, ExtensionSpanEvent,
    ExtensionSpanKind, ExtensionSpanRecord, SpanRole, ToolSpanAnnotations, TraceAttributeWriter,
    TraceCarrier, TraceContext, TraceEvent, TraceLimits, TraceOmission, TraceOmissionCounts,
    TraceOmissionDiagnostic, TraceOmissionSite, TraceParent, TracePosition, TraceRecord,
    TraceSpanKind, TraceTimestamp, TraceValue, WriterRemaining,
};

use super::super::{TraceEmitter, TraceStanding};
use super::{BoundCallback, SharedHold};

/// One open span of a body, by the order it was opened in.
pub(crate) type NodeId = u64;

/// The fixed cost of one hold entry, charged beside its record's bytes.
pub(crate) const HOLD_ENTRY_BYTES: u64 = std::mem::size_of::<Held>() as u64;

/// A closing record held until its parent publishes, with what it charged
/// its account.
pub(super) struct Held {
    pub(super) record: TraceRecord,
    pub(super) charge: u64,
}

impl Held {
    fn fresh(record: TraceRecord) -> Self {
        Self { record, charge: 0 }
    }

    /// What holding `record` charges: its serialized size and the entry's.
    fn cost(record: &TraceRecord) -> u64 {
        serde_json::to_vec(record).map_or(0, |bytes| bytes.len() as u64) + HOLD_ENTRY_BYTES
    }
}

/// An open extension span: what it buffered, and the closing records of
/// its children held until it publishes.
struct OpenNode {
    parent: Option<NodeId>,
    span: TraceCarrier,
    parent_span: TraceCarrier,
    started_at_ms: u64,
    name: String,
    kind: ExtensionSpanKind,
    operation: Option<ExtensionOperation>,
    writer: TraceAttributeWriter,
    events: Vec<ExtensionSpanEvent>,
    content: Vec<ExtensionContent>,
    error_type: Option<String>,
    omissions: TraceOmissionCounts,
    held: Vec<Held>,
}

/// What publishing a span needs once it left the body's map: its record's
/// parts and the records it held.
pub(super) struct Finished {
    node: NodeId,
    parent: Option<NodeId>,
    position: TracePosition,
    record: ExtensionSpanRecord,
    omissions: TraceOmissionCounts,
    held: Vec<Held>,
}

struct Phase {
    closed: bool,
    nodes: BTreeMap<NodeId, OpenNode>,
    /// Spans ended whose own record is not delivered yet (a shift holds it
    /// behind its journal frontier), with the children delivered meanwhile:
    /// they publish right after it.
    undelivered: BTreeMap<NodeId, Vec<Held>>,
    next: NodeId,
    /// Extension and point spans opened.
    opened: u32,
    /// Bytes the body's contributions charged.
    bytes: u64,
    omissions: TraceOmissionCounts,
    tool_span: Option<ToolSpanState>,
}

/// The body's annotations of its own tool span (§7.4 rule 2).
struct ToolSpanState {
    writer: TraceAttributeWriter,
    set: u32,
    annotations: ToolSpanAnnotations,
    sink: Arc<Mutex<ToolSpanAnnotations>>,
}

/// One body execution's tracing state, shared by every clone of its
/// capability. Only lash constructs one, and its [`BodyTrace`] guard
/// revokes it on every exit.
pub(crate) struct BodyTraceState {
    standing: TraceStanding,
    anchor: Option<TraceParent>,
    context: TraceContext,
    callback: OnceLock<BoundCallback>,
    call: Option<crate::CallAddress>,
    /// The unanchored cell the body runs in, while the turn holds its model
    /// response: records under it are held by the cell (§3.8 P5).
    cell: Option<(crate::HeldResponse, String)>,
    limits: TraceLimits,
    account: SharedHold,
    phase: Mutex<Phase>,
}

impl BodyTraceState {
    pub(super) fn new(spec: super::BodySpec) -> Arc<Self> {
        let limits = spec.standing.runtime().limits();
        let account = match &spec.cell {
            Some((held, _)) => held.trace_hold(limits.publication_hold()),
            None => SharedHold::new(limits.publication_hold()),
        };
        let tool_span = spec.tool_span.map(|sink| {
            *sink.lock_recover() = ToolSpanAnnotations::default();
            ToolSpanState {
                writer: TraceAttributeWriter::new(TraceSpanKind::Tool, &limits),
                set: 0,
                annotations: ToolSpanAnnotations::default(),
                sink,
            }
        });
        let state = Arc::new(Self {
            standing: spec.standing,
            anchor: spec.anchor,
            context: spec.context,
            callback: OnceLock::new(),
            call: spec.call,
            cell: spec.cell,
            limits,
            account,
            phase: Mutex::new(Phase {
                closed: false,
                nodes: BTreeMap::new(),
                undelivered: BTreeMap::new(),
                next: 0,
                opened: 0,
                bytes: 0,
                omissions: TraceOmissionCounts::default(),
                tool_span,
            }),
        });
        if let Some(callback) = spec.callback {
            state.bind(callback);
        }
        state
    }

    /// Binds the registered callback the body runs: once, by the router or
    /// wrapper that invokes it. A later binding changes nothing.
    pub(crate) fn bind(&self, callback: BoundCallback) {
        let _ = self.callback.set(callback);
    }

    pub(super) fn standing(&self) -> &TraceStanding {
        &self.standing
    }

    /// The W3C context of the body's own tool span; `None` for a body
    /// that is not a tool attempt's.
    pub(super) fn tool_span_context(&self) -> Option<TraceCarrier> {
        let tool_body = self.phase.lock_recover().tool_span.is_some();
        tool_body
            .then(|| self.standing.container().cloned())
            .flatten()
    }

    pub(super) fn anchor(&self) -> Option<&TraceParent> {
        self.anchor.as_ref()
    }

    pub(super) fn context(&self) -> &TraceContext {
        &self.context
    }

    pub(super) fn cell(&self) -> Option<&(crate::HeldResponse, String)> {
        self.cell.as_ref()
    }

    pub(super) fn call(&self) -> Option<&crate::CallAddress> {
        self.call.as_ref()
    }

    /// Whether the body is still running and its spans of `kind` would be
    /// built.
    pub(super) fn records(&self, kind: TraceSpanKind) -> bool {
        self.callback.get().is_some()
            && self.limits.extension_spans() > 0
            && self.standing.builds(kind)
            && !self.phase.lock_recover().closed
    }

    /// The body's own address: the callback it was bound from, the span its
    /// capability was bound at, and the attempt it ran for.
    pub(super) fn site(&self) -> TraceOmissionSite {
        let (owner, key) = self.callback.get().map_or_else(Default::default, |bound| {
            (bound.identity.owner.clone(), bound.identity.key.clone())
        });
        TraceOmissionSite::Body {
            owner,
            key,
            at: self.standing.container().map(TraceCarrier::span_id),
            call: self.call.as_ref().map(|address| address.call_id.clone()),
            attempt: self.call.as_ref().map(|address| address.attempt),
        }
    }

    pub(super) fn diagnostic(&self) -> TraceOmissionDiagnostic {
        TraceOmissionDiagnostic {
            site: self.site(),
            counts: self.phase.lock_recover().omissions,
        }
    }

    /// What is left of the body's budget: the spans it may still open and
    /// the bytes its contributions may still charge.
    pub(super) fn remaining(&self) -> WriterRemaining {
        let phase = self.phase.lock_recover();
        WriterRemaining {
            attributes: self.limits.extension_spans().saturating_sub(phase.opened),
            bytes: self.limits.extension_bytes().saturating_sub(phase.bytes),
        }
    }

    fn now(&self) -> u64 {
        self.standing.runtime().clock().timestamp_ms()
    }

    /// Counts `omission` against the body (and its open span `node`): on
    /// its diagnostic while it runs, straight into the runtime's ledger
    /// once its exit reported that diagnostic.
    fn omit(&self, phase: &mut Phase, node: Option<NodeId>, omission: TraceOmission) {
        if phase.closed {
            self.standing
                .runtime()
                .omissions()
                .record_one(&self.site(), omission);
            return;
        }
        phase.omissions.add(omission);
        if let Some(node) = node.and_then(|node| phase.nodes.get_mut(&node)) {
            node.omissions.add(omission);
        }
    }

    /// Counts `omission` against the body from outside any span.
    pub(super) fn count(&self, omission: TraceOmission) {
        self.omit(&mut self.phase.lock_recover(), None, omission);
    }

    /// Charges `bytes` to the body's budget, all or nothing.
    fn charge(&self, phase: &mut Phase, bytes: u64) -> Result<(), TraceOmission> {
        if phase.bytes.saturating_add(bytes) > self.limits.extension_bytes() {
            return Err(TraceOmission::BodyBytes);
        }
        phase.bytes += bytes;
        Ok(())
    }

    /// Opens a span (or, with `point`, mints a point) named `name` under
    /// `parent`, the open span the capability stands at, else the anchored
    /// container at `parent_span`. A refusal mints nothing.
    pub(super) fn open(
        self: &Arc<Self>,
        parent: Option<NodeId>,
        parent_span: &TraceCarrier,
        spec: lash_trace::SpanSpec,
    ) -> Result<(NodeId, TraceCarrier, u64), TraceOmission> {
        let (name, kind, operation) = spec.into_parts();
        let mut phase = self.phase.lock_recover();
        let result = self.admit_open(&mut phase, &name);
        if let Err(omission) = result {
            self.omit(&mut phase, parent, omission);
            return Err(omission);
        }
        let span = TraceCarrier::new(
            parent_span.trace_id(),
            self.standing.leaf_span_id(),
            parent_span.flags(),
            parent_span.tracestate().clone(),
        );
        let started_at_ms = self.now();
        let id = phase.next;
        phase.next += 1;
        let parent = parent.filter(|parent| phase.nodes.contains_key(parent));
        phase.nodes.insert(
            id,
            OpenNode {
                parent,
                span: span.clone(),
                parent_span: parent_span.clone(),
                started_at_ms,
                name: name.into_owned(),
                kind,
                operation,
                writer: TraceAttributeWriter::new(TraceSpanKind::Extension, &self.limits),
                events: Vec::new(),
                content: Vec::new(),
                error_type: None,
                omissions: TraceOmissionCounts::default(),
                held: Vec::new(),
            },
        );
        Ok((id, span, started_at_ms))
    }

    fn admit_open(&self, phase: &mut Phase, name: &str) -> Result<(), TraceOmission> {
        if phase.closed {
            return Err(TraceOmission::Late);
        }
        if phase.opened >= self.limits.extension_spans() {
            return Err(TraceOmission::Spans);
        }
        let bytes = name.len() as u64;
        if name.is_empty() || bytes > self.limits.name_bytes() {
            return Err(TraceOmission::Name);
        }
        self.charge(phase, bytes)?;
        phase.opened += 1;
        Ok(())
    }

    /// Writes one attribute (or, with `event`, the attribute of the event
    /// being built) of the open span `node` through its writer, charging the
    /// body's budget first.
    pub(super) fn attr(
        &self,
        node: NodeId,
        key: lash_trace::AttrKey,
        value: TraceValue,
    ) -> Result<(), TraceOmission> {
        let mut phase = self.phase.lock_recover();
        let result = self.admit_attr(&mut phase, node, key, value);
        if let Err(omission) = result {
            self.omit(&mut phase, Some(node), omission);
        }
        result
    }

    fn admit_attr(
        &self,
        phase: &mut Phase,
        node: NodeId,
        key: lash_trace::AttrKey,
        value: TraceValue,
    ) -> Result<(), TraceOmission> {
        if phase.closed || !phase.nodes.contains_key(&node) {
            return Err(TraceOmission::Late);
        }
        let bytes = key.as_str().len() as u64 + bounded_len(&value, &self.limits);
        if phase.bytes.saturating_add(bytes) > self.limits.extension_bytes() {
            return Err(TraceOmission::BodyBytes);
        }
        let open = phase.nodes.get_mut(&node).ok_or(TraceOmission::Late)?;
        let result = open.writer.set(key, value);
        if matches!(result, Ok(()) | Err(TraceOmission::ValueTruncated)) {
            phase.bytes += bytes;
        }
        result
    }

    /// Adds one content value to the open span `node`: charged to the body
    /// before §5.5's truncation, which the emitter applies.
    pub(super) fn content(
        &self,
        node: NodeId,
        key: lash_trace::AttrKey,
        value: serde_json::Value,
    ) -> Result<(), TraceOmission> {
        let mut phase = self.phase.lock_recover();
        let result = (|| {
            if phase.closed || !phase.nodes.contains_key(&node) {
                return Err(TraceOmission::Late);
            }
            let name = key.as_str();
            if name.is_empty()
                || name.len() as u64 > self.limits.attribute_key_bytes()
                || !name.bytes().all(|byte| byte.is_ascii_graphic())
            {
                return Err(TraceOmission::Key);
            }
            let bytes = name.len() as u64
                + serde_json::to_vec(&value).map_or(u64::MAX, |bytes| bytes.len() as u64);
            self.charge(&mut phase, bytes)?;
            let open = phase.nodes.get_mut(&node).ok_or(TraceOmission::Late)?;
            open.content.push(ExtensionContent {
                key: name.to_owned(),
                value,
            });
            Ok(())
        })();
        if let Err(omission) = result {
            self.omit(&mut phase, Some(node), omission);
        }
        result
    }

    /// Records one event on the open span `node`, its attributes written by
    /// `attrs` under the event's own cap.
    pub(super) fn event(
        &self,
        node: NodeId,
        name: String,
        attrs: impl FnOnce(&mut lash_trace::SpanAttributes),
    ) -> Result<(), TraceOmission> {
        let at_ms = self.now();
        let mut phase = self.phase.lock_recover();
        let result = (|| {
            if phase.closed || !phase.nodes.contains_key(&node) {
                return Err(TraceOmission::Late);
            }
            if phase.nodes[&node].events.len() as u64 >= u64::from(self.limits.events_per_span()) {
                return Err(TraceOmission::Events);
            }
            if name.is_empty() || name.len() as u64 > self.limits.name_bytes() {
                return Err(TraceOmission::Name);
            }
            let mut attributes = lash_trace::SpanAttributes::new(&self.limits);
            attrs(&mut attributes);
            let (attributes, refused) = attributes.finish();
            let bytes = name.len() as u64
                + attributes
                    .iter()
                    .map(|attribute| attribute.key.len() as u64 + attribute.value.encoded_len())
                    .sum::<u64>();
            self.charge(&mut phase, bytes)?;
            let open = phase.nodes.get_mut(&node).ok_or(TraceOmission::Late)?;
            open.events.push(ExtensionSpanEvent {
                name,
                at_ms,
                attributes,
            });
            open.omissions.merge(&refused);
            phase.omissions.merge(&refused);
            Ok(())
        })();
        if let Err(omission) = result {
            self.omit(&mut phase, Some(node), omission);
        }
        result
    }

    /// Marks the open span `node` failed with `error_type`.
    pub(super) fn fail(&self, node: NodeId, error_type: String) -> Result<(), TraceOmission> {
        let mut phase = self.phase.lock_recover();
        let result = (|| {
            if phase.closed || !phase.nodes.contains_key(&node) {
                return Err(TraceOmission::Late);
            }
            if error_type.is_empty() || error_type.len() as u64 > self.limits.name_bytes() {
                return Err(TraceOmission::Name);
            }
            self.charge(&mut phase, error_type.len() as u64)?;
            if let Some(open) = phase.nodes.get_mut(&node) {
                open.error_type = Some(error_type);
            }
            Ok(())
        })();
        if let Err(omission) = result {
            self.omit(&mut phase, Some(node), omission);
        }
        result
    }

    /// Records a point named `name` under `parent` (or the anchored
    /// container), its attributes written by `attrs`.
    pub(super) fn point(
        self: &Arc<Self>,
        parent: Option<NodeId>,
        parent_span: &TraceCarrier,
        name: std::borrow::Cow<'static, str>,
        attrs: impl FnOnce(&mut lash_trace::SpanAttributes),
    ) {
        let Ok((node, _, _)) = self.open(parent, parent_span, lash_trace::SpanSpec::internal(name))
        else {
            return;
        };
        let mut attributes = lash_trace::SpanAttributes::new(&self.limits);
        attrs(&mut attributes);
        let (attributes, refused) = attributes.finish();
        {
            let mut phase = self.phase.lock_recover();
            for (omission, count) in refused.iter() {
                phase.omissions.add_n(omission, count);
            }
            for attribute in attributes {
                let _ = self
                    .admit_attr(
                        &mut phase,
                        node,
                        lash_trace::AttrKey::from(attribute.key),
                        attribute.value,
                    )
                    .map_err(|omission| self.omit(&mut phase, Some(node), omission));
            }
        }
        if let Some(finished) = self.take(node, None) {
            self.publish_span(finished);
        }
    }

    /// Ends the open span `node` (`ended_by`). An explicit end of a span the
    /// body's exit or hold pressure already closed is counted late; a drop
    /// of its handle afterwards is not an end and counts nothing.
    pub(super) fn end(self: &Arc<Self>, node: NodeId, ended_by: ExtensionSpanEnd) {
        match self.take(node, Some(ended_by)) {
            Some(finished) => self.publish_span(finished),
            None if ended_by == ExtensionSpanEnd::End => self.count(TraceOmission::Late),
            None => {}
        }
    }

    /// Takes `node` out of the body's map and finishes its record, under the
    /// body's mutex: the linearization point of an end against revocation.
    fn take(&self, node: NodeId, ended_by: Option<ExtensionSpanEnd>) -> Option<Finished> {
        let ended_at_ms = self.now();
        let mut phase = self.phase.lock_recover();
        if phase.closed {
            return None;
        }
        let open = phase.nodes.remove(&node)?;
        Some(self.finish(&mut phase, node, open, ended_by, ended_at_ms))
    }

    fn finish(
        &self,
        phase: &mut Phase,
        node: NodeId,
        open: OpenNode,
        ended_by: Option<ExtensionSpanEnd>,
        ended_at_ms: u64,
    ) -> Finished {
        let point = ended_by.is_none();
        let ended_at_ms = if point {
            open.started_at_ms
        } else {
            ended_at_ms
        };
        let (callback_owner, callback_key) =
            self.callback.get().map_or_else(Default::default, |bound| {
                (bound.identity.owner.clone(), bound.identity.key.clone())
            });
        // The writer's refusals were counted on the span as they happened.
        let (contributions, _) = open.writer.finish();
        let omissions = open.omissions;
        phase.undelivered.insert(node, Vec::new());
        Finished {
            node,
            parent: open.parent,
            position: TracePosition {
                span: open.span,
                parent: Some(open.parent_span),
                links: lash_trace::TraceLinks::new(),
                role: SpanRole::Whole,
                kind: if point {
                    TraceSpanKind::Point
                } else {
                    TraceSpanKind::Extension
                },
                started_at: TraceTimestamp::from_unix_ms(open.started_at_ms),
                ended_at: Some(TraceTimestamp::from_unix_ms(ended_at_ms)),
                scope: self.standing.scope().map(|scope| scope.scope.clone()),
                attempt: self.standing.attempt().cloned(),
            },
            record: ExtensionSpanRecord {
                plugin_id: callback_owner,
                callback: callback_key,
                name: open.name,
                kind: open.kind,
                operation: open.operation,
                started_at_ms: open.started_at_ms,
                ended_at_ms,
                point,
                error_type: open.error_type,
                ended_by,
                attributes: contributions
                    .added
                    .into_iter()
                    .map(|(key, value)| ExtensionAttribute {
                        key: key.as_str().to_owned(),
                        value,
                    })
                    .collect(),
                events: open.events,
                content: open.content,
            },
            omissions,
            held: open.held,
        }
    }

    /// Emits a finished span's record and releases what it held after it.
    fn publish_span(self: &Arc<Self>, finished: Finished) {
        let Finished {
            node,
            parent,
            position,
            record,
            omissions,
            held,
        } = finished;
        let body = Arc::clone(self);
        let mut held = Some(held);
        let deliver = move |mut built: TraceRecord, _: &TraceEmitter| {
            built.omissions.merge(&omissions);
            let mut batch = vec![Held::fresh(built)];
            batch.extend(held.take().unwrap_or_default());
            let delivered = body.phase.lock_recover().undelivered.remove(&node);
            batch.extend(delivered.unwrap_or_default());
            body.release(parent, batch);
        };
        self.standing.emit_whole(
            position,
            self.context.clone(),
            TraceEvent::ExtensionSpan(record),
            deliver,
        );
    }

    /// Releases `batch`, a closing record followed by what it held, whose
    /// parent is the open span `parent` (else the anchored container): held
    /// in that span while it is open, published otherwise. A hold that
    /// would pass the account's cap finalizes the parent and its open
    /// ancestors early instead, so a closing record is never dropped for a
    /// body hold (§3.8 P5).
    pub(super) fn release(self: &Arc<Self>, parent: Option<NodeId>, mut batch: Vec<Held>) {
        let Some(first) = batch.first_mut() else {
            return;
        };
        let mut phase = self.phase.lock_recover();
        if let Some(waiting) = parent.and_then(|parent| phase.undelivered.get_mut(&parent)) {
            // The parent ended but its record waits behind the frontier.
            waiting.extend(batch);
            return;
        }
        let Some(parent) = parent.filter(|parent| phase.nodes.contains_key(parent)) else {
            drop(phase);
            self.publish(batch);
            return;
        };
        let cost = Held::cost(&first.record);
        if self.account.admit(cost) {
            first.charge = cost;
            if let Some(open) = phase.nodes.get_mut(&parent) {
                open.held.extend(batch);
            }
            return;
        }
        if self.cell_is_unanchored() {
            // Finalizing the chain would hold it under the cell in the same
            // account: the cell's strategy sheds the arriving extension
            // record with its own hold. A lash closing record is never shed.
            let shed = matches!(first.record.event, TraceEvent::ExtensionSpan(_));
            if shed {
                drop(phase);
                self.shed(batch);
            } else {
                self.account.force(cost);
                first.charge = cost;
                if let Some(open) = phase.nodes.get_mut(&parent) {
                    open.held.extend(batch);
                }
            }
            return;
        }
        // Finalize the parent with every open ancestor that gates it,
        // outermost first, each with what it held.
        let mut chain = Vec::new();
        let mut next = Some(parent);
        while let Some(id) = next.filter(|id| phase.nodes.contains_key(id)) {
            chain.push(id);
            next = phase.nodes.get(&id).and_then(|open| open.parent);
        }
        chain.reverse();
        // The chain is cut when the record it could not hold began, so each
        // span ends before that record does.
        let cut_ms = batch[0]
            .record
            .position
            .as_ref()
            .map_or(u64::MAX, |position| position.started_at.unix_ms())
            .min(self.now());
        let mut finished = Vec::new();
        for id in chain {
            if let Some(open) = phase.nodes.remove(&id) {
                let ended_at_ms = cut_ms.max(open.started_at_ms);
                let end = Some(ExtensionSpanEnd::HoldPressure);
                finished.push(self.finish(&mut phase, id, open, end, ended_at_ms));
            }
        }
        drop(phase);
        for finished in finished {
            self.publish_span(finished);
        }
        // The parent is published now: the record follows it.
        self.publish(batch);
    }

    fn cell_is_unanchored(&self) -> bool {
        self.cell
            .as_ref()
            .is_some_and(|(held, _)| held.committed_as().is_none())
    }

    /// Publishes `batch` in order: under the cell while the turn still holds
    /// its model response, else to the sinks now.
    fn publish(&self, batch: Vec<Held>) {
        if let Some((held, _)) = self
            .cell
            .as_ref()
            .filter(|(held, _)| held.committed_as().is_none())
        {
            // The batch's first record is new to the account unless a body
            // hold charged it; the rest were charged where they were held.
            let new_cost = batch
                .first()
                .filter(|first| first.charge == 0)
                .map_or(0, |first| Held::cost(&first.record));
            let charged = batch.iter().filter(|held| held.charge > 0).count();
            let charges = batch.iter().map(|held| held.charge).sum::<u64>();
            let account = self.account.clone();
            let emitter = self.standing.runtime().emitter().clone();
            let records = batch
                .into_iter()
                .map(|held| held.record)
                .collect::<Vec<_>>();
            let count = records.len();
            let admitted = held.hold_trace(
                &self.account,
                (new_cost > 0).then_some(new_cost),
                Box::new(move || {
                    for record in &records {
                        emitter.append(record);
                    }
                    account.refund(charged + usize::from(new_cost > 0), charges + new_cost);
                }),
            );
            if !admitted {
                self.account.refund(charged, charges);
                self.shed_count(count);
            }
            return;
        }
        let emitter = self.standing.runtime().emitter();
        for held in batch {
            emitter.append(&held.record);
            self.account
                .refund(usize::from(held.charge > 0), held.charge);
        }
    }

    /// Sheds `batch`, a record and its own hold: nothing published names it.
    fn shed(&self, batch: Vec<Held>) {
        let charged = batch.iter().filter(|held| held.charge > 0).count();
        self.account
            .refund(charged, batch.iter().map(|held| held.charge).sum());
        self.shed_count(batch.len());
    }

    fn shed_count(&self, count: usize) {
        if let Some((_, graph_key)) = &self.cell {
            let mut counts = TraceOmissionCounts::default();
            counts.add_n(
                TraceOmission::PublicationHold,
                u32::try_from(count).unwrap_or(u32::MAX),
            );
            self.standing.runtime().omissions().record(
                &TraceOmissionSite::Cell {
                    graph_key: graph_key.clone(),
                },
                &counts,
            );
        }
    }

    /// Revokes the body's right: every still-open span is closed now
    /// (`ended_by`), outermost first with what it held, and the body's
    /// omissions are reported once.
    pub(super) fn revoke(self: &Arc<Self>, ended_by: ExtensionSpanEnd) {
        let ended_at_ms = self.now();
        let (finished, counts) = {
            let mut phase = self.phase.lock_recover();
            if phase.closed {
                return;
            }
            phase.closed = true;
            let nodes = std::mem::take(&mut phase.nodes);
            let finished = nodes
                .into_iter()
                .map(|(node, open)| {
                    self.finish(&mut phase, node, open, Some(ended_by), ended_at_ms)
                })
                .collect::<Vec<_>>();
            (finished, phase.omissions)
        };
        for finished in finished {
            self.publish_span(finished);
        }
        if counts.is_empty() {
            return;
        }
        let diagnostic = TraceOmissionDiagnostic {
            site: self.site(),
            counts,
        };
        self.standing
            .runtime()
            .omissions()
            .record(&diagnostic.site, &diagnostic.counts);
        let context = self.context.clone();
        self.standing
            .observe(|| (context, TraceEvent::BodyOmissions(diagnostic)));
    }

    /// Sets one annotation of the body's own tool span (§7.4 rule 2).
    pub(super) fn annotate(
        &self,
        key: lash_trace::AttrKey,
        value: TraceValue,
    ) -> Result<(), TraceOmission> {
        let mut phase = self.phase.lock_recover();
        let result = self.admit_annotation(&mut phase, key, value);
        if let Err(omission) = result {
            self.omit(&mut phase, None, omission);
        }
        result
    }

    fn admit_annotation(
        &self,
        phase: &mut Phase,
        key: lash_trace::AttrKey,
        value: TraceValue,
    ) -> Result<(), TraceOmission> {
        if phase.closed {
            return Err(TraceOmission::Late);
        }
        if phase.tool_span.is_none() {
            return Err(TraceOmission::DisallowedKey);
        }
        let name = key.as_str().to_owned();
        if !lash_trace::tool_span_key_allowed(&name) {
            return Err(TraceOmission::DisallowedKey);
        }
        let refinable = lash_trace::RefinableKey::of_key(&name);
        let bytes = name.len() as u64 + bounded_len(&value, &self.limits);
        if phase.bytes.saturating_add(bytes) > self.limits.extension_bytes() {
            return Err(TraceOmission::BodyBytes);
        }
        let cap = self.limits.tool_span_attributes();
        let Some(state) = phase.tool_span.as_mut() else {
            return Err(TraceOmission::DisallowedKey);
        };
        let earlier = state
            .annotations
            .0
            .iter()
            .any(|attribute| attribute.key == name);
        if refinable.is_none() && !earlier && state.set >= cap {
            return Err(TraceOmission::Attributes);
        }
        let result = match refinable {
            Some(refinable) => state.writer.replace(refinable, value.clone()),
            None => state.writer.set(key, value.clone()),
        };
        if !matches!(result, Ok(()) | Err(TraceOmission::ValueTruncated)) {
            return result;
        }
        let kept = match refinable {
            Some(refinable) => state
                .writer
                .contributions()
                .replaced
                .iter()
                .find(|(replaced, _)| *replaced == refinable)
                .map(|(_, value)| value.clone()),
            None => state
                .writer
                .contributions()
                .added
                .iter()
                .find(|(added, _)| added.as_str() == name)
                .map(|(_, value)| value.clone()),
        }
        .unwrap_or(value);
        match state
            .annotations
            .0
            .iter_mut()
            .find(|attribute| attribute.key == name)
        {
            Some(attribute) => attribute.value = kept,
            None => {
                if refinable.is_none() {
                    state.set += 1;
                }
                state.annotations.0.push(ExtensionAttribute {
                    key: name,
                    value: kept,
                });
            }
        }
        *state.sink.lock_recover() = state.annotations.clone();
        phase.bytes += bytes;
        result
    }
}

/// The bytes `value` charges once bounded: a string cut to
/// `attribute_value_bytes`, a JSON value over it nothing (the writer refuses
/// it whole), every other value whole.
fn bounded_len(value: &TraceValue, limits: &TraceLimits) -> u64 {
    let limit = limits.attribute_value_bytes();
    match value {
        TraceValue::Str(value) => (value.len() as u64).min(limit),
        TraceValue::StrArray(values) => values
            .iter()
            .map(|value| (value.len() as u64).min(limit) + 1)
            .sum(),
        TraceValue::Json(_) if value.encoded_len() > limit => 0,
        value => value.encoded_len(),
    }
}

/// Where a leaf opened inside an open extension span publishes its closing
/// record: held in that span until it publishes (P3), at once after.
#[derive(Clone)]
pub(crate) struct HoldGate {
    body: Arc<BodyTraceState>,
    node: NodeId,
}

impl HoldGate {
    pub(super) fn new(body: Arc<BodyTraceState>, node: NodeId) -> Self {
        Self { body, node }
    }

    /// Holds or publishes `record`, a leaf's closing record.
    pub(crate) fn release(&self, record: TraceRecord, _: &TraceEmitter) {
        self.body
            .release(Some(self.node), vec![Held::fresh(record)]);
    }
}

/// The guard of one body execution: owned by the wrapper that runs the
/// body, never handed to it. Its exit, by return, cancellation or unwind,
/// revokes the body's right and closes what is still open.
pub(crate) struct BodyTrace {
    state: Option<Arc<BodyTraceState>>,
}

impl BodyTrace {
    pub(super) fn new(state: Option<Arc<BodyTraceState>>) -> Self {
        Self { state }
    }

    /// Binds the registered callback the body runs.
    pub(crate) fn bind(&self, callback: BoundCallback) {
        if let Some(state) = &self.state {
            state.bind(callback);
        }
    }

    /// The guard of a body that is not traced.
    pub(crate) fn untraced() -> Self {
        Self { state: None }
    }

    /// The body returned: what is still open closes `body_exit`.
    pub(crate) fn finish(mut self) {
        if let Some(state) = self.state.take() {
            state.revoke(ExtensionSpanEnd::BodyExit);
        }
    }

    /// The body panicked and its wrapper contained the panic: what is still
    /// open closes `panic`.
    pub(crate) fn panicked(mut self) {
        if let Some(state) = self.state.take() {
            state.revoke(ExtensionSpanEnd::Panic);
        }
    }
}

impl BodyTrace {
    /// Runs `body` under this guard: its return revokes with `body_exit`, a
    /// panic in a poll with `panic` before the panic leaves, and a drop
    /// before its return with `cancelled`, before anything the body holds
    /// is dropped.
    pub(crate) fn guard<F: std::future::Future>(self, body: F) -> BodyGuarded<F> {
        BodyGuarded {
            trace: Some(self),
            body: Box::pin(body),
        }
    }
}

/// A body run under its [`BodyTrace`] ([`BodyTrace::guard`]).
pub(crate) struct BodyGuarded<F> {
    trace: Option<BodyTrace>,
    body: std::pin::Pin<Box<F>>,
}

impl<F: std::future::Future> std::future::Future for BodyGuarded<F> {
    type Output = F::Output;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let polled =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.body.as_mut().poll(cx)));
        match polled {
            Ok(std::task::Poll::Ready(output)) => {
                if let Some(trace) = self.trace.take() {
                    trace.finish();
                }
                std::task::Poll::Ready(output)
            }
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(payload) => {
                if let Some(trace) = self.trace.take() {
                    trace.panicked();
                }
                std::panic::resume_unwind(payload)
            }
        }
    }
}

impl<F> Drop for BodyGuarded<F> {
    fn drop(&mut self) {
        // Revoked before the body's own state, and the handles in it, drop.
        drop(self.trace.take());
    }
}

impl Drop for BodyTrace {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            state.revoke(if std::thread::panicking() {
                ExtensionSpanEnd::Panic
            } else {
                ExtensionSpanEnd::Cancelled
            });
        }
    }
}

#[cfg(test)]
impl super::HoldAccount {
    pub(crate) fn records(&self) -> u32 {
        self.records
    }
}

#[cfg(test)]
impl BodyTraceState {
    /// The body's hold account now: a probe for the pressure laws.
    pub(crate) fn account(&self) -> super::HoldAccount {
        self.account.snapshot()
    }
}
