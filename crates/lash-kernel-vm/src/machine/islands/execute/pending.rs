//! Cleanup control names a departure kind. Its value has a distinct slab
//! lifetime, including while another return is being evaluated in cleanup.

use super::*;

#[derive(Clone, Copy)]
pub(super) enum Departure {
    Normal,
    Break,
    Continue,
    Return,
    ReturnVia(StmtId),
    Throw,
}

#[derive(Clone, Copy)]
pub(super) enum Phase {
    Body,
    Catch,
    Finally(Departure),
}

pub(super) struct Departing {
    pub(super) how: Departure,
    pub(super) value: Value,
}

impl Departing {
    pub(super) fn from_source(source: &Completion) -> Self {
        let (how, value) = match source {
            Completion::Normal => (Departure::Normal, Value::Null),
            Completion::Break => (Departure::Break, Value::Null),
            Completion::Continue => (Departure::Continue, Value::Null),
            Completion::Return(value) => (Departure::Return, **value),
            Completion::ReturnVia { value, stmt } => (Departure::ReturnVia(*stmt), **value),
            Completion::Throw(value) => (Departure::Throw, **value),
        };
        Self { how, value }
    }

    pub(super) fn project(&self) -> Completion {
        match self.how {
            Departure::Normal => Completion::Normal,
            Departure::Break => Completion::Break,
            Departure::Continue => Completion::Continue,
            Departure::Return => Completion::Return(Box::new(self.value)),
            Departure::ReturnVia(stmt) => Completion::ReturnVia {
                value: Box::new(self.value),
                stmt,
            },
            Departure::Throw => Completion::Throw(Box::new(self.value)),
        }
    }
}

impl Phase {
    pub(super) fn of(source: &TryPhase) -> (Self, Value) {
        match source {
            TryPhase::Body => (Self::Body, Value::Null),
            TryPhase::Catch => (Self::Catch, Value::Null),
            TryPhase::Finally(source) => {
                let departure = Departing::from_source(source);
                (Self::Finally(departure.how), departure.value)
            }
        }
    }

    pub(super) fn project(self, value: Value) -> TryPhase {
        match self {
            Self::Body => TryPhase::Body,
            Self::Catch => TryPhase::Catch,
            Self::Finally(how) => TryPhase::Finally(Departing { how, value }.project()),
        }
    }

    pub(super) fn needs_source(self) -> bool {
        matches!(
            self,
            Self::Catch | Self::Finally(Departure::Throw | Departure::ReturnVia(_))
        )
    }
}
