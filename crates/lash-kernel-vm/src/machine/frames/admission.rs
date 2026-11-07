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
