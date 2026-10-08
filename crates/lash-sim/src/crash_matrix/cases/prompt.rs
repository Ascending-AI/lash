//! The prompt seam (ADR 0133 §6, FIG-5255): one turn, behind the facade on
//! the production driver, whose three model calls compose the memo plugin's
//! section through the frame plugin's wrapper ([`crate::crash_matrix::prompts`]).
//! The first call sets T, the second proposes a T its plugin's check
//! denies, and an `AfterWork` checkpoint counts each round.
//!
//! Laws:
//! - T-NEXT: the call after `set_t` renders the new T, through the wrapper,
//!   over a session view that shows `set_t`'s outcome: the cut is the last
//!   commit at the call, not the iteration's sync.
//! - NO-SPECULATION: the denied T is never rendered and never committed.
//! - Decisions at admission: the checkpoint counts `model.start` commits
//!   with each call survive every cut after it.
//! - Identity (CRASH-CUTS): every attempt of a call carries the one prompt
//!   its admission committed, in the exact body it admitted: one body per
//!   call at every cut.
//! - NO-PRODUCER-RESUME: once a call is admitted, nothing produces it
//!   again. No render or wrapping of a call follows its first send, and no
//!   before-turn callback follows the turn's first send: a resume serves the
//!   turn's recorded before-turn decisions and the admitted body.
//! - Snapshot: every call's snapshot commits with its admission and reads
//!   back, with no renderer, as the base text, the wrapping and the final
//!   text the model received.
//! - Composition: uncut, every call composes once. Cut at its `model.start`,
//!   a call whose admission landed composes once (a resend composes
//!   nothing) and one whose admission did not land composes again. Any
//!   other cut wastes at most one composition, made before the admission.
//! - State across the cut: a cut between `set_t`'s `round.outcome` and the
//!   next call's admission still renders the T `set_t` committed (FIG-5266
//!   commits it with the outcome).

use std::sync::{Arc, Mutex};

use lash_core::llm::types::{LlmContentBlock, LlmRequest, LlmRole};
use lash_core::prompt_sections::PromptPlacement;
use lash_core::sync::MutexExt as _;
use lash_core_store::store::RunTerminalKind;
use lash_durable::CommitLabel;
use lash_durable_test::{Cut, Fault, SimNodes};
use lash_sansio::SessionId;

use super::{admit_turn, turn_end, turn_settled};
use crate::crash_matrix::deployment::Workload;
use crate::crash_matrix::prompts::{
    BEFORE_TURN, NEXT_T, RENDERED, SENT, WRAPPED, committed_memo, memo_prompt, memo_section,
};
use crate::crash_matrix::services::TurnScript;
use crate::crash_matrix::world::World;

/// The turn's model calls.
const CALLS: u32 = 3;

#[derive(Default)]
pub struct PromptCase {
    tag: String,
    session: Mutex<Option<SessionId>>,
}

impl PromptCase {
    /// The case with its session named apart by `tag`.
    #[must_use]
    pub fn tagged(tag: &str) -> Self {
        Self {
            tag: tag.to_owned(),
            session: Mutex::default(),
        }
    }

    fn session(&self) -> Option<SessionId> {
        self.session.lock_recover().clone()
    }
}

/// The prompt call `call` must carry: T unset, then set by `set_t`, with one
/// more checkpoint counted before each call after the first, and `set_t`'s
/// outcome in the session view from the call after it.
fn expected(call: u32) -> String {
    match call {
        1 => memo_prompt("none", 0, false),
        call => memo_prompt(NEXT_T, u64::from(call - 1), true),
    }
}

/// Read the received request at the section's recorded placement (ADR 0133).
/// Current context must be in the trailing User message, not elsewhere in
/// the instructions or conversation.
fn prompt_at(request: &LlmRequest, placement: PromptPlacement) -> Option<String> {
    match placement {
        PromptPlacement::InitialInstructions => request.instructions.as_deref().map(str::to_owned),
        PromptPlacement::CurrentContext => {
            let tail = request.messages.last()?;
            if tail.role != LlmRole::User {
                return None;
            }
            Some(
                tail.blocks
                    .iter()
                    .filter_map(|block| match block {
                        LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
        PromptPlacement::Excluded => None,
    }
}

/// The call a `model.start` cut admits: a turn's first call starts at
/// `model.start`, and each later one with its round's presentation.
fn cut_call(cut: &Cut) -> Option<u32> {
    let nth = u32::try_from(cut.point.nth).ok()?;
    if cut.point.label == CommitLabel::MODEL_START {
        Some(nth)
    } else if cut.point.label == CommitLabel::ROUND_PRESENT_MODEL_START {
        Some(nth + 1)
    } else {
        None
    }
}

#[async_trait::async_trait]
impl Workload for PromptCase {
    async fn seed(&self, world: &Arc<World>, _nodes: &Arc<SimNodes>) -> Result<(), String> {
        let session = admit_turn(world, TurnScript::Prompt, &format!("prompt{}", self.tag)).await?;
        *self.session.lock_recover() = Some(session);
        Ok(())
    }

    async fn done(&self, _world: &World, nodes: &SimNodes) -> bool {
        match self.session() {
            Some(session) => turn_settled(nodes, &session).await,
            None => false,
        }
    }

    async fn laws(&self, world: &World, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let Some(session) = self.session() else {
            return vec!["the turn was never seeded".to_owned()];
        };
        let mut violations = Vec::new();
        match turn_end(nodes, &session).await {
            Some(end) if end.kind() == RunTerminalKind::Answered => {}
            other => violations.push(format!("the prompt turn ended {other:?}")),
        }
        let notes = world.notes();
        let sent_prefix = format!("{SENT} {session} call=");
        let rendered_prefix = format!("{RENDERED} {session} call=");
        let wrapped_prefix = format!("{WRAPPED} {session} call=");
        let before_turn = format!("{BEFORE_TURN} {session}");
        violations.extend(no_producer_resume(
            &notes,
            &sent_prefix,
            &[&rendered_prefix, &wrapped_prefix],
            &before_turn,
        ));
        // T-NEXT, NO-SPECULATION, decisions at admission and identity: every
        // attempt of call k carries call k's one prompt.
        let mut sent: [Vec<LlmRequest>; CALLS as usize] = std::array::from_fn(|_| Vec::new());
        for note in &notes {
            let Some(rest) = note.strip_prefix(&sent_prefix) else {
                continue;
            };
            let Some((head, request_json)) = rest.split_once(" :: ") else {
                violations.push(format!("prompt: unreadable note {note}"));
                continue;
            };
            let call = head
                .split_whitespace()
                .next()
                .and_then(|call| call.parse::<u32>().ok())
                .unwrap_or(0);
            if !(1..=CALLS).contains(&call) {
                violations.push(format!("prompt: the model saw a call {call}: {note}"));
                continue;
            }
            match serde_json::from_str(request_json) {
                Ok(request) => sent[call as usize - 1].push(request),
                Err(error) => violations.push(format!(
                    "prompt: call {call} has an unreadable received request: {error}"
                )),
            }
        }
        let counts = sent.each_ref().map(Vec::len);
        if counts.contains(&0) {
            violations.push(format!("prompt: the model saw calls {counts:?} times"));
        }
        // Identity: every attempt of a call sends the body its admission
        // committed.
        for call in 1..=CALLS {
            let mut bodies = notes
                .iter()
                .filter_map(|note| note.strip_prefix(&sent_prefix))
                .filter(|rest| {
                    rest.split_whitespace()
                        .next()
                        .is_some_and(|head| head == call.to_string())
                })
                .filter_map(|rest| {
                    rest.split_whitespace()
                        .find_map(|field| field.strip_prefix("body="))
                })
                .collect::<Vec<_>>();
            bodies.sort_unstable();
            bodies.dedup();
            if bodies.len() > 1 {
                violations.push(format!(
                    "prompt identity: call {call} was sent {} distinct bodies",
                    bodies.len()
                ));
            }
        }
        // Composition: once per call, and again only for a call cut before
        // its admission landed.
        for call in 1..=CALLS {
            let of_call = |prefix: &str| {
                notes
                    .iter()
                    .filter(|note| {
                        note.strip_prefix(prefix)
                            .is_some_and(|rest| rest == call.to_string())
                    })
                    .count()
            };
            let composed = of_call(&rendered_prefix);
            let wrapped = of_call(&wrapped_prefix);
            if wrapped != composed {
                violations.push(format!(
                    "prompt composition: call {call} rendered {composed} times and was wrapped \
                     {wrapped} times"
                ));
            }
            // One cut wastes at most one composition of a call, and only one
            // made before the call's admission landed: an owner fenced or
            // killed before its `model.start` committed.
            let bounds = match cut {
                None => 1..=1,
                Some(cut) if cut_call(cut) == Some(call) => match cut.fault {
                    Fault::CommitThenAbort | Fault::AckHidden => 1..=1,
                    Fault::FailBefore | Fault::Abort => 2..=2,
                    _ => 1..=2,
                },
                Some(_) => 1..=2,
            };
            if !bounds.contains(&composed) {
                violations.push(format!(
                    "prompt composition: call {call} composed {composed} times, not {bounds:?}"
                ));
            }
        }
        // Every call's snapshot committed with its admission and reads back
        // with no renderer: the memo section's base text, the frame's
        // wrapping and the final text the model received.
        let reads = nodes.database();
        for call in 1..=CALLS {
            let key = lash_durable::domain::PromptCallKey {
                session: session.clone(),
                call: lash_durable::domain::ModelCallId::Turn {
                    run: super::run_of(&session),
                    ordinal: call,
                },
            };
            let loaded =
                match lash_core::plugin::prompt::load_admitted_call(reads.as_ref(), &key).await {
                    Ok(Some(lash_core::plugin::prompt::LoadedAdmittedCall {
                        prompt: Some(loaded),
                        ..
                    })) => loaded,
                    other => {
                        violations.push(format!("prompt snapshot: call {call} loaded {other:?}"));
                        continue;
                    }
                };
            // The protocol's own sections record beside the memo's.
            let memo = memo_section();
            let mut recorded = loaded
                .snapshot
                .sections
                .iter()
                .filter(|section| section.section == memo);
            let (Some(section), None) = (recorded.next(), recorded.next()) else {
                violations.push(format!(
                    "prompt snapshot: call {call} did not record the memo section once"
                ));
                continue;
            };
            let base = loaded.text(&section.base).unwrap_or_default();
            let value = loaded.text(&section.value).unwrap_or_default();
            let wrapped = section
                .wraps
                .iter()
                .map(|wrap| loaded.text(&wrap.output).unwrap_or_default())
                .collect::<Vec<_>>();
            if wrapped != [value] || value != format!("{base} (framed)") {
                violations.push(format!(
                    "prompt snapshot: call {call} recorded base {base:?}, wraps {wrapped:?} and \
                     value {value:?}, not the frame's wrapping"
                ));
            }
            // T-NEXT, NO-SPECULATION, decisions at admission, identity and
            // snapshot: inspect every attempt at its committed placement.
            for request in &sent[call as usize - 1] {
                let received = prompt_at(request, section.placement);
                if !received
                    .as_ref()
                    .is_some_and(|text| text.contains(&expected(call)))
                {
                    violations.push(format!(
                        "prompt T: call {call} was sent {received:?} at {:?}, not {:?}",
                        section.placement,
                        expected(call)
                    ));
                }
                if !received.as_ref().is_some_and(|text| text.contains(value)) {
                    violations.push(format!(
                        "prompt snapshot: call {call} recorded value {value:?} at {:?}, \
                         which the model did not receive",
                        section.placement
                    ));
                }
            }
        }
        // The turn commits the T it set and every checkpoint decision; the
        // denied T never publishes.
        match committed_memo(world, session.as_str()).await {
            Ok((Some(t), checkpoints)) if t == NEXT_T && checkpoints == u64::from(CALLS - 1) => {}
            other => violations.push(format!(
                "prompt memo: the turn committed {other:?}, not T={NEXT_T} with {} checkpoints",
                CALLS - 1
            )),
        }
        violations
    }
}

/// NO-PRODUCER-RESUME over the notes in the order they happened: after a
/// call's first send no producer of that call runs (a note under any of
/// `producers` naming it), and after the turn's first send no before-turn
/// callback runs (`before_turn`). The turn's before-turn callback ran at
/// least once.
fn no_producer_resume(
    notes: &[String],
    sent_prefix: &str,
    producers: &[&str],
    before_turn: &str,
) -> Vec<String> {
    let mut violations = Vec::new();
    let mut sent = std::collections::BTreeSet::new();
    let mut before_turns = 0;
    for note in notes {
        if let Some(rest) = note.strip_prefix(sent_prefix) {
            if let Some(call) = rest.split_whitespace().next() {
                sent.insert(call.to_owned());
            }
            continue;
        }
        if note == before_turn {
            before_turns += 1;
            if !sent.is_empty() {
                violations.push(
                    "no producer resume: a before-turn callback ran after the turn's first send"
                        .to_owned(),
                );
            }
            continue;
        }
        for prefix in producers {
            if let Some(call) = note.strip_prefix(prefix)
                && sent.contains(call)
            {
                violations.push(format!(
                    "no producer resume: {note} ran after call {call} was sent"
                ));
            }
        }
    }
    if before_turns == 0 {
        violations.push("no producer resume: the before-turn callback never ran".to_owned());
    }
    violations
}
