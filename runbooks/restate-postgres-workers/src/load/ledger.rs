//! Closed load witness discriminants shared by writers, readers and generators.
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerDiscriminantError(pub String);
impl fmt::Display for LedgerDiscriminantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown or illegal witness discriminant {}", self.0)
    }
}
impl std::error::Error for LedgerDiscriminantError {}

macro_rules! names {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $name { $(#[serde(rename = $wire)] $variant),+ }
        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            pub const fn as_str(self) -> &'static str { match self { $(Self::$variant => $wire),+ } }
        }
        impl FromStr for $name {
            type Err = LedgerDiscriminantError;
            fn from_str(raw: &str) -> Result<Self, Self::Err> { match raw { $($wire => Ok(Self::$variant)),+, _ => Err(LedgerDiscriminantError(raw.into())) } }
        }
    };
}
names!(WitnessedOperation { Behaviors => "behaviors", Turn => "turn", DeleteSession => "delete-session", CronSetup => "cron-setup", CronTick => "cron-tick" });
impl WitnessedOperation {
    pub fn of(request: &super::LoadRequest) -> Self {
        match request {
            super::LoadRequest::Behaviors { .. } => Self::Behaviors,
            super::LoadRequest::Turn { .. } => Self::Turn,
            super::LoadRequest::DeleteSession { .. } => Self::DeleteSession,
            super::LoadRequest::CronSetup { .. } => Self::CronSetup,
            super::LoadRequest::CronTick { .. } => Self::CronTick,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadEvidence {
    Sent(WitnessedOperation),
    Terminal(WitnessedOperation),
    AttachmentPut,
    AttachmentRead,
}
impl LoadEvidence {
    pub fn operation(self) -> &'static str {
        match self {
            Self::Sent(op) | Self::Terminal(op) => op.as_str(),
            Self::AttachmentPut | Self::AttachmentRead => "attachment",
        }
    }
    pub const fn phase(self) -> &'static str {
        match self {
            Self::Sent(_) => "sent",
            Self::Terminal(_) => "terminal",
            Self::AttachmentPut => "put",
            Self::AttachmentRead => "read",
        }
    }
    pub fn pairs() -> Vec<(&'static str, &'static str)> {
        WitnessedOperation::ALL
            .iter()
            .flat_map(|op| [Self::Sent(*op), Self::Terminal(*op)])
            .chain([Self::AttachmentPut, Self::AttachmentRead])
            .map(|row| (row.operation(), row.phase()))
            .collect()
    }
}
impl FromStr for LoadEvidence {
    type Err = LedgerDiscriminantError;
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.split_once(':') {
            Some(("attachment", "put")) => Ok(Self::AttachmentPut),
            Some(("attachment", "read")) => Ok(Self::AttachmentRead),
            Some((op, "sent")) => Ok(Self::Sent(op.parse()?)),
            Some((op, "terminal")) => Ok(Self::Terminal(op.parse()?)),
            _ => Err(LedgerDiscriminantError(raw.into())),
        }
    }
}

names!(CampaignPhase { Started => "started", Complete => "complete", Failed => "failed" });
names!(InjectionPhase { Intent => "intent", Injected => "injected", Recovered => "recovered", Failed => "failed" });
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultFamily {
    Campaign,
    Fault,
    Upgrade,
}

macro_rules! faults {
    ($($variant:ident($phase:ident) => ($wire:literal, $family:ident)),+ $(,)?) => {
        names!(FaultKind { $($variant => $wire),+ });
        impl FaultKind {
            pub const fn family(self) -> FaultFamily { match self { $(Self::$variant => FaultFamily::$family),+ } }
            pub fn steps(family: FaultFamily) -> impl Iterator<Item = Self> { Self::ALL.iter().copied().filter(move |kind| kind.family() == family) }
        }
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum FaultEvidence { $($variant($phase)),+ }
        impl FaultEvidence {
            pub const fn kind(self) -> FaultKind { match self { $(Self::$variant(_) => FaultKind::$variant),+ } }
            pub const fn phase(self) -> &'static str { match self { $(Self::$variant(phase) => phase.as_str()),+ } }
            pub fn pairs() -> Vec<(&'static str, &'static str)> { { let mut pairs = Vec::new(); $(pairs.extend($phase::ALL.iter().map(|phase| ($wire, phase.as_str())));)+ pairs } }
        }
        impl FromStr for FaultEvidence {
            type Err = LedgerDiscriminantError;
            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                match raw.split_once(':') { $(Some(($wire, phase)) => Ok(Self::$variant(phase.parse()?))),+, _ => Err(LedgerDiscriminantError(raw.into())) }
            }
        }
    };
}
faults! {
    Campaign(CampaignPhase) => ("campaign", Campaign),
    WorkerKill(InjectionPhase) => ("worker-kill", Fault),
    RestateRestart(InjectionPhase) => ("restate-restart", Fault),
    RollingDeploy(InjectionPhase) => ("rolling-deploy", Fault),
    HalfRoll(InjectionPhase) => ("half-roll", Upgrade),
    Rollback(InjectionPhase) => ("rollback", Upgrade),
    Roll(InjectionPhase) => ("roll", Upgrade),
    Finalize(InjectionPhase) => ("finalize", Upgrade),
    Fence(InjectionPhase) => ("fence", Upgrade),
}

pub fn fault_classes() -> Vec<&'static str> {
    std::iter::once("fault-campaign")
        .chain(FaultKind::steps(FaultFamily::Fault).map(FaultKind::as_str))
        .collect()
}
pub fn upgrade_classes() -> Vec<&'static str> {
    std::iter::once("upgrade-campaign")
        .chain(FaultKind::steps(FaultFamily::Upgrade).map(FaultKind::as_str))
        .chain(std::iter::once("sessions-through-roll"))
        .collect()
}

pub fn contract() -> serde_json::Value {
    serde_json::json!({"events": LoadEvidence::pairs(), "faults": FaultEvidence::pairs(),
        "classes": super::verify::CLASSES, "fault_classes": fault_classes(), "upgrade_classes": upgrade_classes(),
        "upgrade_steps": FaultKind::steps(FaultFamily::Upgrade).map(FaultKind::as_str).collect::<Vec<_>>()})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_ledger_pair_round_trips_and_crossed_pairs_are_refused() {
        for (op, phase) in LoadEvidence::pairs() {
            let row: LoadEvidence = format!("{op}:{phase}").parse().expect("event pair");
            assert_eq!((row.operation(), row.phase()), (op, phase));
        }
        for (kind, phase) in FaultEvidence::pairs() {
            let row: FaultEvidence = format!("{kind}:{phase}").parse().expect("fault pair");
            assert_eq!((row.kind().as_str(), row.phase()), (kind, phase));
        }
        for bad in ["turn:put", "attachment:sent", "trun:terminal"] {
            assert!(bad.parse::<LoadEvidence>().is_err());
        }
        for bad in ["campaign:injected", "worker-kill:started", "roll:complete"] {
            assert!(bad.parse::<FaultEvidence>().is_err());
        }
    }
}
