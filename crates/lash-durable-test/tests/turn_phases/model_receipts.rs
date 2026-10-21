//! Admission, receipt recovery and re-send laws for the scripted turn.

use super::*;

/// Admission at `model.start` (FIG-5255), in every cell.
///
/// - IDENTITY: the turn's first call is call 1 and its second call 2, and
///   every attempt of a call carries that call's one composition.
/// - FENCE and CUT-ACK: every attempt the model receives is of an admitted
///   call: the turn's row, read as the attempt is sent, pins that call and
///   stores the composition the attempt carries.
/// - WIRE-SLOTS (FIG-5259, FIG-5445): every attempt sends the template the
///   call's admission stored, so a resend on a second owner, whose renderer
///   and provider builder would produce other bytes, sends the admitted
///   literals and slots; a call with no slot sends exactly its admitted
///   bytes, and a slot is filled by one value its codec made from that
///   attempt's own delivery ([`slots::fill_laws`]). Two attempts of one call
///   differ only inside their slots.
pub(super) fn admission_laws(seen: &Seen) -> Vec<String> {
    let mut violations = Vec::new();
    for call in &seen.calls {
        let ordinal = if call.second { 2 } else { 1 };
        match &call.stored {
            None => violations.push(format!(
                "WIRE-SLOTS: attempt {} of call {ordinal} was sent with no stored admission",
                call.attempt
            )),
            Some(stored) => {
                if call.sent_template.as_ref() != Some(&**stored) {
                    violations.push(format!(
                        "WIRE-SLOTS: attempt {} of call {ordinal} sent template {:?} while its \
                         admission stored {}",
                        call.attempt,
                        call.sent_template
                            .as_ref()
                            .map(RecordedRequestTemplate::redacted),
                        stored.redacted()
                    ));
                }
                if stored.slots().next().is_none() && call.sent != stored.redacted() {
                    violations.push(format!(
                        "WIRE: attempt {} of call {ordinal} sent {} while its admission stored {}",
                        call.attempt,
                        call.sent,
                        stored.redacted()
                    ));
                }
                violations.extend(slots::fill_laws(ordinal, call, stored));
            }
        }
        match call.composed {
            Some(composed) if composed.call == ordinal => {}
            other => violations.push(format!(
                "IDENTITY: attempt {} of call {ordinal} carried composition {other:?}",
                call.attempt
            )),
        }
        if call.admitted != Some((ordinal, true)) {
            violations.push(format!(
                "FENCE: attempt {} of call {ordinal} was sent while the rows admitted {:?}",
                call.attempt, call.admitted
            ));
        }
    }
    for ordinal in [1, 2] {
        let mut sent = seen
            .calls
            .iter()
            .filter(|call| {
                call.composed
                    .is_some_and(|composed| composed.call == ordinal)
            })
            .filter_map(|call| call.composed);
        if let Some(first) = sent.next()
            && sent.any(|other| other != first)
        {
            violations.push(format!(
                "IDENTITY: call {ordinal} was sent with two compositions: {:?}",
                seen.calls
            ));
        }
        let mut templates = seen
            .calls
            .iter()
            .filter(|call| u32::from(call.second) + 1 == ordinal)
            .map(|call| &call.sent_template);
        if let Some(first) = templates.next()
            && templates.any(|other| other != first)
        {
            violations.push(format!(
                "WIRE-SLOTS: call {ordinal} was sent with two templates: {:?}",
                seen.calls
            ));
        }
    }
    violations
}

/// MATERIAL (FIG-5259): an admitted call whose stored admission is gone is
/// never sent again, and nothing rebuilds it: it settles unsent as
/// `admitted_request_unavailable`, and the turn ends with that failure.
pub(super) fn material_laws(seen: &Seen) -> Vec<String> {
    let mut violations = Vec::new();
    if !seen.material_lost {
        violations.push("MATERIAL: no send lost its call's admission".to_owned());
    }
    if seen.calls.len() != 1 {
        violations.push(format!(
            "MATERIAL: the call was sent {} times after its admission was lost: {:?}",
            seen.calls.len(),
            seen.calls
        ));
    }
    let failed = seen
        .told
        .iter()
        .any(|told| told.contains("admitted_request_unavailable"));
    if !failed {
        violations.push(format!(
            "MATERIAL: the turn did not end with admitted_request_unavailable: {:?}",
            seen.told
        ));
    }
    violations
}

/// How often call `call` composed its prompt.
pub(super) fn compositions_of(seen: &Seen, call: u32) -> usize {
    seen.compositions
        .iter()
        .filter(|composition| composition.call == call)
        .count()
}

/// The laws of one cut: a node killed after `model.start` committed, and
/// how often a call cut at its `model.start` composed (FIG-5255).
pub(super) fn cut_laws(mode: Mode, cut: &Cut, seen: &Seen) -> Vec<String> {
    let mut violations = Vec::new();
    if matches!(
        mode,
        Mode::Plain | Mode::Slots | Mode::AdapterSlots | Mode::SlotHold
    ) && cut.point.label == CommitLabel::MODEL_START
    {
        // Uncut up to its cut, a plain turn's nth `model.start` admits its
        // nth call. A crash after admission resends the admitted call and
        // composes nothing; one before it composes the call again.
        let call = u32::try_from(cut.point.nth).unwrap();
        let expected = match cut.fault {
            Fault::CommitThenAbort | Fault::AckHidden => Some(1),
            Fault::FailBefore | Fault::Abort => Some(2),
            _ => None,
        };
        let composed = compositions_of(seen, call);
        if let Some(expected) = expected
            && composed != expected
        {
            violations.push(format!(
                "admission: call {call} cut at model.start under {:?} composed {composed} times, \
                 not {expected}",
                cut.fault
            ));
        }
    }
    // An acknowledged scripted receipt survives owner loss just like a
    // production answer: recovery derives it and never sends it again.
    if mode == Mode::Plain
        && cut.point.label == CommitLabel::MODEL_ANSWER
        && matches!(cut.fault, Fault::CommitThenAbort | Fault::AckHidden)
        && seen.calls.len() != 2
    {
        violations.push(format!(
            "receipt: an answered call was sent again after {:?}: {:?}",
            cut.fault, seen.calls
        ));
    }
    let killed_after_pin =
        cut.point.label == CommitLabel::MODEL_START && matches!(cut.fault, Fault::CommitThenAbort);
    if !killed_after_pin {
        return violations;
    }
    let max_attempt = seen
        .calls
        .iter()
        .map(|call| call.attempt)
        .max()
        .unwrap_or(0);
    match mode {
        Mode::Plain | Mode::Slots | Mode::AdapterSlots | Mode::SlotHold => {
            if max_attempt != 2 {
                violations.push(format!(
                    "re-send: a call pinned by a killed node was not re-sent as attempt 2: {:?}",
                    seen.calls
                ));
            }
        }
        Mode::ShortDeadline => {
            if max_attempt > 1 {
                violations.push(format!(
                    "L-C1: a call whose pinned deadline passed was sent again: {:?}",
                    seen.calls
                ));
            }
        }
        Mode::CancelWhileStreaming
        | Mode::AfterStepWhileStreaming
        | Mode::QueuedCancel
        | Mode::CancelAtAdmission
        | Mode::MaterialLost
        | Mode::MaterialStale
        | Mode::CollidingCommit
        | Mode::OutsideWriterRange
        | Mode::CheckpointFault
        | Mode::CancelWhileDeriving => {}
    }
    violations
}
