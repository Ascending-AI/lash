//! Action continuation coordinates and retained roles.
use super::*;
use lash_kernel_exec::coord::{ActionEntry, ActionKind, ActionPhase, StatementCoordinate};

pub(super) fn action_kind(kind: &NestedKind) -> Option<ActionKind> {
    Some(match kind {
        NestedKind::Yield => ActionKind::Yield,
        NestedKind::Cancel | NestedKind::CancelEffect => return None,
        NestedKind::Call(_) => ActionKind::Call,
        NestedKind::Body(_) => ActionKind::Body,
        NestedKind::Perform { then_yield, .. } => ActionKind::Perform {
            then_yield: *then_yield,
        },
        NestedKind::Spawn(_) => ActionKind::Spawn,
        NestedKind::Join => ActionKind::Join,
        NestedKind::JoinMany(_) => ActionKind::JoinMany,
    })
}

/// The page forms of `roles`, in their order: what a projection writes.
pub(super) fn role_records(
    arena: &mut Arena,
    access: &Access<'_>,
    roles: &[Role],
) -> Result<Vec<RoleRecord>, HeapFault> {
    roles
        .iter()
        .map(|role| {
            Ok(RoleRecord {
                coordinate: RoleCoordinate::Expression(role.site.to_vec()),
                value: project(arena, access, &role.value)?,
            })
        })
        .collect()
}

/// The arena values of the saved `roles`, in their order.
pub(super) fn admit_roles(
    arena: &mut Arena,
    access: &Access<'_>,
    roles: Vec<RoleRecord>,
) -> Result<Vec<Role>, HeapFault> {
    roles
        .into_iter()
        .map(|role| {
            Ok(Role {
                holding: lash_kernel_state::kernel_internals::logical::Holding::from_page(
                    access,
                    &role.value,
                )?,
                value: admit(arena, access, &role.value)?,
                site: match role.coordinate {
                    RoleCoordinate::Expression(site) => site.into(),
                    RoleCoordinate::Source { .. } => {
                        return Err(malformed("an action retained a non-expression role"));
                    }
                },
            })
        })
        .collect()
}

pub(super) fn action_phase(phase: ContinuationPhase) -> Option<ActionPhase> {
    Some(match phase {
        ContinuationPhase::Yielded => ActionPhase::Yielded,
        ContinuationPhase::Call { phase: 0 } => ActionPhase::Called,
        ContinuationPhase::Waiting => ActionPhase::Waiting,
        ContinuationPhase::Spawned => ActionPhase::Spawned,
        ContinuationPhase::Joining => ActionPhase::Joining,
        ContinuationPhase::Call { .. }
        | ContinuationPhase::Source { .. }
        | ContinuationPhase::Completion { .. } => return None,
    })
}

/// The resume entry of `pending`, the continuation a frame saved where it
/// stands at an action nested in statement `stmt`, at `site`, admitted by
/// the coordinate grammar beside `arrival` (`K-EVAL-008`): its action names
/// one of the statement's own nested actions under this layout.
pub(super) fn continuation(
    exe: &Executable,
    stmt: StmtId,
    site: &Site,
    pending: &Pending,
    arrival: Arrival,
) -> Result<ActIx, RestoreRefusal> {
    let (stream, _, _) = exe.statement(stmt);
    let depth = site.path.len();
    let action = SitePath(pending.site[..].into());
    let full = Site::new(
        site.unit.clone(),
        site.path
            .iter()
            .chain(pending.site.iter())
            .copied()
            .collect::<Vec<_>>(),
    );
    // The entry must be the statement's own: an action in a closure
    // written inside it belongs to the closure's statement.
    let Some((act, nested)) = stream
        .nested_at
        .get(&(stmt, full))
        .map(|act| (*act, &stream.nested[act.ix()]))
        .filter(|(_, nested)| nested.depth as usize == depth)
    else {
        return Err(RestoreRefusal::UnknownSite { action });
    };
    let phase = action_phase(pending.phase).ok_or_else(|| RestoreRefusal::UnknownSite {
        action: action.clone(),
    })?;
    let Some(kind) = action_kind(&nested.kind) else {
        return Err(RestoreRefusal::IllegalPhase { action, phase });
    };
    let entry = ActionEntry {
        action: action.clone(),
        kind,
        roles: nested
            .roles
            .iter()
            .map(|role| SitePath(role.site.path[depth..].into()))
            .collect(),
    };
    let saved = StatementCoordinate::Nested {
        action,
        phase,
        roles: pending
            .roles
            .iter()
            .map(|role| SitePath(role.site[..].into()))
            .collect(),
    };
    coord::admit(&entry, &saved, arrival)?;
    Ok(act)
}

/// Region recovery checks source roles separately; task arrival must still
/// agree with the action whose accepted Enter the continuation retains.
pub(super) fn source_arrival(
    exe: &Executable,
    source: &source::SourceContinuation,
    standing: Option<(StmtId, Option<ActIx>)>,
    arrival: Arrival,
) -> Result<(), RestoreRefusal> {
    // A slice can stop after Enter, before the machine serves the action.
    // Such a ready frame has no delivered input to authenticate yet.
    if arrival == Arrival::Fresh && source.progress != 3 {
        return Ok(());
    }
    let action = site_path(&source.at.site);
    let (kind, phase) = if source.progress == 3 {
        (ActionKind::Call, ActionPhase::Called)
    } else if let Some((statement, act)) = standing {
        let kind = if let Some(act) = act {
            let (stream, _, _) = exe.statement(statement);
            let nested = &stream.nested[act.ix()].kind;
            if matches!(nested, NestedKind::Cancel | NestedKind::CancelEffect) {
                return inline_arrival(action, arrival);
            }
            action_kind(nested)
        } else {
            use crate::compile::ActionKind as Kind;
            match &exe.stmt(statement).form {
                Form::Action(action) => Some(match action.kind {
                    Kind::Call { .. } => ActionKind::Call,
                    Kind::Perform { then_yield, .. } => ActionKind::Perform { then_yield },
                    Kind::Yield => ActionKind::Yield,
                    Kind::Spawn { .. } => ActionKind::Spawn,
                    Kind::Join(_) => ActionKind::Join,
                    Kind::JoinMany(..) => ActionKind::JoinMany,
                    Kind::Cancel(_) | Kind::CancelEffect(_) => {
                        return inline_arrival(site_path(&exe.stmt(statement).site), arrival);
                    }
                }),
                _ => None,
            }
        }
        .ok_or_else(|| RestoreRefusal::UnknownSite {
            action: action.clone(),
        })?;
        let phase = match kind {
            ActionKind::Call | ActionKind::Body => ActionPhase::Called,
            ActionKind::Perform { .. } => ActionPhase::Waiting,
            ActionKind::Yield => ActionPhase::Yielded,
            ActionKind::Spawn => ActionPhase::Spawned,
            ActionKind::Join | ActionKind::JoinMany => ActionPhase::Joining,
        };
        (kind, phase)
    } else {
        return if matches!(arrival, Arrival::Fresh | Arrival::Raised) {
            Ok(())
        } else {
            Err(RestoreRefusal::TaskState { action })
        };
    };
    // The independent region verifier owns the exact RoleKey set. Use the
    // coordinate grammar's arrival relation without inventing expression roles.
    let entry = ActionEntry {
        action: action.clone(),
        kind,
        roles: Box::new([]),
    };
    let saved = StatementCoordinate::Nested {
        action,
        phase,
        roles: Box::new([]),
    };
    coord::admit(&entry, &saved, arrival)
}

// Cancellation answers synchronously with null, or raises, without a turn.
fn inline_arrival(action: SitePath, arrival: Arrival) -> Result<(), RestoreRefusal> {
    if matches!(arrival, Arrival::Valued { handle: false } | Arrival::Raised) {
        Ok(())
    } else {
        Err(RestoreRefusal::TaskState { action })
    }
}
