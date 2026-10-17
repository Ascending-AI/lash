//! The coordinate grammar: how a saved run names where it stands, and how
//! restore maps that back into the code it runs under
//! (`docs/design/kernel-end-state.md`, seam A).
//!
//! A saved coordinate is semantic only: sites as path suffixes under a
//! frame's statement, phases, roles by the site of the operand that
//! produced them, table cursors by insertion ordinal. It never holds a pc,
//! a register, an arena handle or a compiled table's index, so any layout
//! of any build of the same document restores it. The code's side table
//! states, per statement, the coordinates that can stand there
//! ([`ActionEntry`] for each nested action); restore checks every saved
//! coordinate against it and fails closed with a [`RestoreRefusal`].
//!
//! `lash-kernel-vm` maps its saved records to these coordinates and
//! admits each through [`admit`], [`admit_loop`], [`admit_cleanup`] and
//! [`admit_arguments`] as it restores a frame.

use std::collections::BTreeSet;

/// A site path suffix under a frame's statement site: how a coordinate
/// names an action or an operand.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SitePath(pub Box<[u32]>);

/// The phase an action stands at (`K-EVAL-008`): today's `PhaseRecord`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActionPhase {
    /// A `yield`: the task awaits its queued turn.
    Yielded,
    /// A call: the callee's frame is above.
    Called,
    /// A `perform` or a `sleep`: the task waits on its request.
    Waiting,
    /// A `spawn`: the child runs first (`K-TASK-002`).
    Spawned,
    /// A `join` on one handle or a list (`K-TASK-008`).
    Joining,
}

/// What a nested action is, as the code's side table states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActionKind {
    Yield,
    /// A call whose callee runs in a frame of its own.
    Call,
    /// A library call inside an expression whose function runs its body.
    Body,
    Perform {
        then_yield: bool,
    },
    Sleep {
        then_yield: bool,
    },
    Spawn,
    Join,
    JoinMany,
}

impl ActionKind {
    /// Whether the task takes one more turn once the action's outcome is
    /// in (`K-TASK-005`).
    pub const fn then_yield(self) -> bool {
        matches!(
            self,
            Self::Perform { then_yield: true } | Self::Sleep { then_yield: true }
        )
    }

    /// Whether an action of this kind can stand at `phase`.
    pub const fn stands_at(self, phase: ActionPhase) -> bool {
        matches!(
            (phase, self),
            (ActionPhase::Yielded, Self::Yield)
                | (ActionPhase::Called, Self::Call | Self::Body)
                | (
                    ActionPhase::Waiting,
                    Self::Perform { .. } | Self::Sleep { .. }
                )
                | (ActionPhase::Spawned, Self::Spawn)
                | (ActionPhase::Joining, Self::Join | Self::JoinMany)
        )
    }
}

/// How a task comes to its top frame on resume, from its saved state:
/// what a frame's phase must agree with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Arrival {
    /// The frame stands between statements, and nothing resumes it.
    Fresh,
    /// The frame above it has returned or raised into it.
    Returned,
    /// The task waits on a request, or its outcome is in and its turn taken.
    Requested,
    /// A `then yield` wait's outcome is in and the task's one more turn is
    /// not yet taken (`K-TASK-005`).
    Delivered,
    /// A raise is delivered: a `cancel` at the task's wait or turn.
    Raised,
    /// A value is delivered; `handle` when it is a `spawn`'s.
    Valued { handle: bool },
    /// The task joins, or a join has decided.
    Joined,
}

/// The side table's row for one action nested in a statement: what a
/// saved [`StatementCoordinate::Nested`] must match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionEntry {
    pub action: SitePath,
    pub kind: ActionKind,
    /// The operands the statement has evaluated when the action is reached
    /// and still uses, by site, in evaluation order: `live_roles` of the
    /// action's site. Liveness is semantic: a role an optimized code no
    /// longer reads is still saved and restored.
    pub roles: Box<[SitePath]>,
}

/// Where a frame stands in its statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatementCoordinate {
    /// At a step boundary: the statement is next, or its block has ended.
    Boundary,
    /// Inside the statement, at an action nested in its expression,
    /// keeping the operands it has evaluated as roles.
    Nested {
        action: SitePath,
        phase: ActionPhase,
        roles: Box<[SitePath]>,
    },
}

/// Where a loop's header stands (`K-ITER-005`): today's `HeaderRecord`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LoopPhase {
    /// In the body (a `while` is always here).
    Body,
    /// `open` takes the iterated value to what the loop walks.
    Open,
    /// `next` steps the driver.
    Pull,
    /// `check` runs with a stepped item held.
    Check,
    /// `check` runs at exhaustion, no item held.
    Exhausted,
    /// `item` takes the held item to the value bound.
    Item,
}

/// What a `for` walks, and where it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cursor {
    /// A list's or a tuple's next index.
    Sequence { next: u64 },
    /// A map's keys or a set's members: the insertion ordinal last
    /// visited, `None` before the first; the size the loop opened it at.
    Table { last: Option<u64>, size: u64 },
    /// A text's next code point, by index.
    Text { next: u64 },
    /// A value the protocol's `next` steps.
    Driver,
}

/// One active loop of a frame: its statement, how many iterations it has
/// started (`K-EFF-008` names effects by it), its header's phase, its
/// cursor, and the items its protocol's `check` still allows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoopCoordinate {
    pub site: SitePath,
    pub started: u64,
    pub phase: LoopPhase,
    pub cursor: Option<Cursor>,
    pub remaining: Option<u64>,
}

/// How control is leaving the block a cleanup runs for: today's
/// `EnteredRecord`, with its departing payload kept beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Departure {
    Normal,
    Throw,
    Return,
    Break,
    Continue,
    /// A return whose policy the return site supplies once cleanup unwinds.
    ReturnVia(SitePath),
}

/// One active `try` of a frame and the phase it is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CleanupCoordinate {
    Body {
        site: SitePath,
    },
    Catch {
        site: SitePath,
    },
    Finally {
        site: SitePath,
        departure: Departure,
    },
}

/// One frame's whole coordinate: its statement, its loops and cleanups
/// outermost first, and, for a library body or a formula, the call's
/// retained arguments by position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameCoordinate {
    /// The frame's statement site, from the unit's root.
    pub statement: SitePath,
    pub at: StatementCoordinate,
    pub loops: Box<[LoopCoordinate]>,
    pub cleanups: Box<[CleanupCoordinate]>,
    pub arguments: Option<u32>,
}

/// Why a saved run does not restore into the code it is opened under.
/// Restore fails closed: it never guesses a nearest site or phase.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RestoreRefusal {
    #[error("no action nested in the statement at {action:?}")]
    UnknownSite { action: SitePath },
    #[error("the action at {action:?} cannot stand at {phase:?}")]
    IllegalPhase {
        action: SitePath,
        phase: ActionPhase,
    },
    #[error("the role {role:?} is not saved")]
    MissingRole { role: SitePath },
    #[error("the saved role {role:?} is no role of its action, or is saved twice")]
    ExtraRole { role: SitePath },
    #[error("the task's state disagrees with the action at {action:?}")]
    TaskState { action: SitePath },
    #[error("no loop at {site:?} encloses the frame's statement")]
    UnknownLoop { site: SitePath },
    #[error("the loop at {site:?} cannot stand at {phase:?} with that cursor")]
    IllegalLoop { site: SitePath, phase: LoopPhase },
    #[error("no `try` at {site:?} encloses the frame's statement")]
    UnknownCleanup { site: SitePath },
    #[error("the frame's call keeps {saved} arguments, its function takes {expected}")]
    Arguments { saved: u32, expected: u32 },
}

/// Admits a frame's saved statement coordinate against the side table's
/// `entry` for the action it names, where the task comes to the frame by
/// `arrival` (`K-EVAL-008`). The phase must be one the action's kind can
/// stand at and agree with the arrival, and the saved roles must be
/// exactly the entry's, as a set. A boundary has nothing to admit.
///
/// The caller finds `entry` by the saved action site among the actions
/// nested in the frame's own statement; a site with none is
/// [`RestoreRefusal::UnknownSite`].
pub fn admit(
    entry: &ActionEntry,
    saved: &StatementCoordinate,
    arrival: Arrival,
) -> Result<(), RestoreRefusal> {
    let StatementCoordinate::Nested {
        action,
        phase,
        roles,
    } = saved
    else {
        return Ok(());
    };
    if *action != entry.action {
        return Err(RestoreRefusal::UnknownSite {
            action: action.clone(),
        });
    }
    if !entry.kind.stands_at(*phase) || arrival == Arrival::Fresh {
        return Err(RestoreRefusal::IllegalPhase {
            action: action.clone(),
            phase: *phase,
        });
    }
    let agrees = match phase {
        ActionPhase::Yielded => {
            matches!(arrival, Arrival::Valued { handle: false } | Arrival::Raised)
        }
        ActionPhase::Called => arrival == Arrival::Returned,
        ActionPhase::Waiting => match arrival {
            Arrival::Requested | Arrival::Raised => true,
            // Only a `then yield` wait owes a turn once delivered.
            Arrival::Delivered => entry.kind.then_yield(),
            _ => false,
        },
        ActionPhase::Spawned => {
            matches!(arrival, Arrival::Valued { handle: true } | Arrival::Raised)
        }
        ActionPhase::Joining => matches!(
            arrival,
            Arrival::Joined | Arrival::Valued { .. } | Arrival::Raised
        ),
    };
    if !agrees {
        return Err(RestoreRefusal::TaskState {
            action: action.clone(),
        });
    }
    let mut saved = BTreeSet::new();
    if let Some(twice) = roles.iter().find(|role| !saved.insert(*role)) {
        return Err(RestoreRefusal::ExtraRole {
            role: twice.clone(),
        });
    }
    let expected: BTreeSet<&SitePath> = entry.roles.iter().collect();
    if let Some(extra) = saved.iter().find(|role| !expected.contains(*role)) {
        return Err(RestoreRefusal::ExtraRole {
            role: (*extra).clone(),
        });
    }
    if let Some(missing) = entry.roles.iter().find(|role| !saved.contains(role)) {
        return Err(RestoreRefusal::MissingRole {
            role: missing.clone(),
        });
    }
    Ok(())
}

/// Admits a frame's saved loop coordinate where restore finds a loop of
/// its statement at `site`: a `for` when `walks`, else a `while`, standing
/// in its header (a protocol hook of a `for`) when `in_header`, else in
/// its body. A `while` holds no cursor and stands in its body; a `for`'s
/// header phase must be one its cursor can stand at (`K-ITER-005`): only
/// `open` before the cursor exists, `next` only on a protocol driver, and
/// a `check` only with the items it still allows, which only a map's or a
/// set's walk keeps.
pub fn admit_loop(
    site: &SitePath,
    walks: bool,
    in_header: bool,
    saved: &LoopCoordinate,
) -> Result<(), RestoreRefusal> {
    if saved.site != *site {
        return Err(RestoreRefusal::UnknownLoop {
            site: saved.site.clone(),
        });
    }
    let guarded = saved.remaining.is_some();
    let legal = in_header != (saved.phase == LoopPhase::Body)
        && if walks {
            match (saved.phase, saved.cursor) {
                (LoopPhase::Open, cursor) => cursor.is_none() && !guarded,
                (_, None) => false,
                (LoopPhase::Pull, Some(cursor)) => cursor == Cursor::Driver,
                (LoopPhase::Check | LoopPhase::Exhausted, Some(_)) => guarded,
                (LoopPhase::Body | LoopPhase::Item, Some(_)) => true,
            }
        } else {
            saved.cursor.is_none() && !guarded
        }
        && (!guarded || matches!(saved.cursor, Some(Cursor::Table { .. })));
    if legal {
        Ok(())
    } else {
        Err(RestoreRefusal::IllegalLoop {
            site: saved.site.clone(),
            phase: saved.phase,
        })
    }
}

/// Admits a frame's saved cleanup coordinate where restore finds a `try`
/// of its statement at `site`.
pub fn admit_cleanup(site: &SitePath, saved: &CleanupCoordinate) -> Result<(), RestoreRefusal> {
    let (CleanupCoordinate::Body { site: saved }
    | CleanupCoordinate::Catch { site: saved }
    | CleanupCoordinate::Finally { site: saved, .. }) = saved;
    if saved == site {
        Ok(())
    } else {
        Err(RestoreRefusal::UnknownCleanup {
            site: saved.clone(),
        })
    }
}

/// Admits the arguments a frame's call retains, where `takes` is how many
/// its library function takes and `None` for a frame that is no library
/// body. A library frame keeps all of them when its formula bills the call,
/// and none when its caller's formula covers it (`K-CHG-007`); any other
/// frame keeps no argument list.
pub fn admit_arguments(saved: Option<u32>, takes: Option<u32>) -> Result<(), RestoreRefusal> {
    match (saved, takes) {
        (None, None) => Ok(()),
        (Some(saved), Some(takes)) if saved == takes || saved == 0 => Ok(()),
        _ => Err(RestoreRefusal::Arguments {
            saved: saved.unwrap_or(0),
            expected: takes.unwrap_or(0),
        }),
    }
}
