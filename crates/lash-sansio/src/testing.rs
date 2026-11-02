//! Journal round trips for the existing protocol laws. The fixture keeps
//! only the original build inputs and the serialized rows a host committed.

use crate::sansio::RecordedSyncs;
use crate::{
    JournalRow, TurnMachine, TurnMachineConfig, TurnProtocol, TurnWindow, UnitTurnProtocol,
};
use lash_protocol_types::{AppendVec, Message, MessageSequence, SessionHistoryRecord};

/// A protocol law's original build inputs and its committed input journal.
/// Every resumed machine is built fresh and replays the production journal.
pub struct JournalReplay<M: TurnProtocol + Clone = UnitTurnProtocol> {
    config: TurnMachineConfig<M>,
    messages: Vec<Message>,
    events: AppendVec<SessionHistoryRecord<M::Event>>,
    offset: usize,
    window: Option<TurnWindow<M::Event>>,
    prepared: Option<MessageSequence>,
    rows: Vec<JournalRow<M::IntentOutcome>>,
}

impl<M: TurnProtocol + Clone> JournalReplay<M> {
    /// Retain the inputs that built the law's first machine.
    pub fn new(
        config: TurnMachineConfig<M>,
        messages: Vec<Message>,
        events: AppendVec<SessionHistoryRecord<M::Event>>,
        offset: usize,
    ) -> Self {
        Self {
            config,
            messages,
            events,
            offset,
            window: None,
            prepared: None,
            rows: Vec::new(),
        }
    }

    /// Build the machine over the recorded committed window.
    #[must_use]
    pub fn with_window(mut self, window: TurnWindow<M::Event>) -> Self {
        self.window = Some(window);
        self
    }

    /// Retain the prompt view the turn's build inputs decided.
    #[must_use]
    pub fn with_prepared_messages(mut self, messages: MessageSequence) -> Self {
        self.prepared = Some(messages);
        self
    }

    /// Build a fresh machine from the original inputs, then start its journal.
    pub fn start(&self) -> TurnMachine<M> {
        let mut machine = match &self.window {
            Some(window) => TurnMachine::in_window(
                self.config.clone(),
                window.clone(),
                window.then(self.messages.clone()),
                self.events.as_slice().to_vec(),
                self.offset,
            ),
            None => TurnMachine::new(
                self.config.clone(),
                self.messages.clone(),
                self.events.clone(),
                self.offset,
            ),
        };
        if let Some(prepared) = &self.prepared {
            machine.adopt_prepared_messages(prepared.clone());
        }
        machine.journal_inputs(0);
        machine
    }

    /// Commit this machine's pending inputs as a serialized row, build a
    /// fresh machine and replay all the committed rows into it.
    ///
    /// # Errors
    ///
    /// Serialization, journal divergence or a typed driver-state refusal.
    pub fn resume(
        &mut self,
        machine: &mut TurnMachine<M>,
        syncs: RecordedSyncs<'_>,
    ) -> Result<TurnMachine<M>, String> {
        let row =
            serde_json::to_vec(&machine.stage_inputs().0).map_err(|error| error.to_string())?;
        self.rows
            .push(serde_json::from_slice(&row).map_err(|error| error.to_string())?);
        let inputs = JournalRow::assemble(self.rows.clone()).map_err(|error| error.to_string())?;
        let mut fresh = self.start();
        fresh
            .replay_inputs(inputs, syncs)
            .map_err(|error| error.to_string())?;
        fresh
            .check_parked_state()
            .map_err(|error| error.to_string())?;
        Ok(fresh)
    }
}
