//! Copy-on-write coordinate carry. Unchanged data trees retain their roots;
//! rewritten records are never published until the embedding owner commits.
use crate::{ParkedRefusal, Rewritten};
use lash_kernel_doc::Correspondence;
use lash_kernel_doc::heap::{HeapFault, PageId, PageRef};
use lash_kernel_value::kernel_internals::{effect_id, effect_parts};
use lash_kernel_value::tokens::encode_body;
use {lash_kernel_value::sites::EffectIdentity, lash_kernel_value::sites::TaskIdentity};

use lash_kernel_state::kernel_internals::{
    metadata::metadata_key,
    tree::{Access, Cell, SealWork, Tree},
};
use lash_kernel_state::paged::*;
use lash_kernel_value::sites::EffectId;
use lash_kernel_value::sites::Site;

/// Infrastructure faults remain distinct from a semantic carry refusal.
#[derive(Debug, thiserror::Error)]
pub enum CarryFault {
    #[error(transparent)]
    Refused(#[from] ParkedRefusal),
    #[error(transparent)]
    Heap(#[from] HeapFault),
}

fn state(detail: impl Into<String>) -> ParkedRefusal {
    ParkedRefusal::StateNotCarried {
        detail: detail.into(),
    }
}
fn site(
    c: &Correspondence,
    from: &Site,
    held: &str,
    helpers: &lash_kernel_doc::BuildHelpers,
) -> Result<Site, ParkedRefusal> {
    if matches!(&from.unit, lash_kernel_value::sites::Unit::Library(function) if helpers.contains(function))
    {
        return Ok(from.clone());
    }
    c.successor(from)
        .cloned()
        .ok_or_else(|| ParkedRefusal::SiteNotCarried {
            site: from.clone(),
            held: held.into(),
        })
}
fn task(
    c: &Correspondence,
    from: &TaskIdentity,
    helpers: &lash_kernel_doc::BuildHelpers,
) -> Result<TaskIdentity, ParkedRefusal> {
    // Walk lineage iteratively: user fan-out may be deeper than the Rust stack.
    let mut steps = Vec::new();
    let mut at = from;
    while let TaskIdentity::Spawned(spawn) = at {
        steps.push((
            site(c, &spawn.site, "task spawn", helpers)?,
            spawn.occurrence,
        ));
        at = &spawn.parent;
    }
    let mut to = TaskIdentity::Main;
    for (site, occurrence) in steps.into_iter().rev() {
        to = TaskIdentity::Spawned(lash_kernel_value::sites::SpawnIdentity {
            parent: std::sync::Arc::new(to),
            site,
            occurrence,
        });
    }
    Ok(to)
}
/// Rekeys a wait without changing its occurrence, loops, or recorded request.
pub fn carry_effect(
    c: &Correspondence,
    from: &EffectId,
    helpers: &lash_kernel_doc::BuildHelpers,
) -> Result<EffectId, ParkedRefusal> {
    let from = effect_parts(from);
    Ok(effect_id(EffectIdentity {
        task: task(c, &from.task, helpers)?,
        site: site(c, &from.site, "wait", helpers)?,
        occurrence: from.occurrence,
        loops: from
            .loops
            .iter()
            .map(|held| {
                Ok(lash_kernel_value::sites::LoopIteration {
                    site: site(c, &held.site, "wait loop", helpers)?,
                    iteration: held.iteration,
                })
            })
            .collect::<Result<_, ParkedRefusal>>()?,
    }))
}
struct Carry<'a, 's> {
    access: Access<'s>,
    correspondence: &'a Correspondence,
    predecessor: lash_kernel_value::LinkId,
    successor: lash_kernel_value::LinkId,
    helpers: &'a lash_kernel_doc::BuildHelpers,
}
impl Carry<'_, '_> {
    fn reference(&self, from: &SiteRef, held: &str) -> Result<SiteRef, CarryFault> {
        let from = from.resolve(&self.access)?;
        Ok(SiteRef::new(
            &site(self.correspondence, &from, held, self.helpers)?,
            &self.access,
        )?)
    }
    fn identity(&self, from: TaskIdentityRef) -> Result<TaskIdentityRef, CarryFault> {
        let resolved = from.resolve(&self.access)?;
        let carried = task(self.correspondence, &resolved, self.helpers)?;
        let mut steps = Vec::new();
        let mut at = &carried;
        while let TaskIdentity::Spawned(spawn) = at {
            steps.push((&spawn.site, spawn.occurrence));
            at = &spawn.parent;
        }
        let mut to = TaskIdentityRef::MAIN;
        for (site, occurrence) in steps.into_iter().rev() {
            to = TaskIdentityRef::spawn(
                to,
                SiteRef::new(site, &self.access)?,
                occurrence,
                &self.access,
            )?;
        }
        Ok(to)
    }
    fn tree<T: Cell>(
        &self,
        rows: &mut Tree<T>,
        mut edit: impl FnMut(&mut T) -> Result<(), CarryFault>,
    ) -> Result<(), CarryFault> {
        // Retain only one record and its changed path, not a whole-image vector.
        for rank in 0..rows.len() {
            let (key, mut value) = rows
                .rank_get(rank, &self.access)?
                .ok_or_else(|| state("missing carry row"))?;
            let before = value.clone();
            edit(&mut value)?;
            if !value.same_version(&before) {
                rows.write(key, value, &self.access)?;
            }
        }
        rows.seal(
            &self.access,
            &mut SealWork::new(SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            }),
        )?;
        Ok(())
    }
    fn check_reference(&self, from: &SiteRef, held: &str) -> Result<(), CarryFault> {
        let resolved = from.resolve(&self.access)?;
        site(self.correspondence, &resolved, held, self.helpers)?;
        Ok(())
    }
    fn visit<T: Cell>(
        &self,
        rows: &Tree<T>,
        mut check: impl FnMut(&T) -> Result<(), CarryFault>,
    ) -> Result<(), CarryFault> {
        for rank in 0..rows.len() {
            let (_, row) = rows
                .rank_get(rank, &self.access)?
                .ok_or_else(|| state("missing preflight row"))?;
            check(&row)?;
        }
        Ok(())
    }
    fn check_frame(&self, frame: &FrameRecord) -> Result<(), CarryFault> {
        if frame.link != self.predecessor {
            return Ok(());
        }
        let old = frame.statement.resolve(&self.access)?;
        if matches!(&old.unit, lash_kernel_value::sites::Unit::Library(function) if self.helpers.contains(function))
        {
            return Ok(());
        }
        let new = site(self.correspondence, &old, "frame statement", self.helpers)?;
        if let Some(pending) = &frame.pending {
            let suffix = |path: &[u32], held| -> Result<(), CarryFault> {
                let full = Site::new(
                    old.unit.clone(),
                    old.path
                        .iter()
                        .copied()
                        .chain(path.iter().copied())
                        .collect::<Vec<_>>(),
                );
                let to = site(self.correspondence, &full, held, self.helpers)?;
                if to.unit != new.unit || !to.path.starts_with(&new.path) {
                    return Err(state(format!("{held} {full} left its statement")).into());
                }
                Ok(())
            };
            suffix(&pending.site, "pending action")?;
            match &pending.roles {
                RolesRecord::Flat(roles) => {
                    for role in roles.iter() {
                        suffix(&role.site, "operand role")?;
                    }
                }
                RolesRecord::Spilled(chunks) => self.visit(chunks, |chunk| {
                    for role in &chunk.0 {
                        suffix(&role.site, "operand role")?;
                    }
                    Ok(())
                })?,
            }
        }
        self.visit(&frame.bindings, |row| {
            self.check_reference(&row.declared, "variable declaration")
        })?;
        self.visit(&frame.loops, |row| self.check_reference(&row.site, "loop"))?;
        self.visit(&frame.finally, |row| {
            self.check_reference(&row.site, "cleanup")?;
            if let EnteredRecord::ReturnVia(held) = &row.entered {
                self.check_reference(held, "return policy")?;
            }
            Ok(())
        })
    }
    fn frame(&self, frame: &mut FrameRecord) -> Result<(), CarryFault> {
        if frame.link != self.predecessor {
            return Ok(());
        }
        let old = frame.statement.resolve(&self.access)?;
        if matches!(&old.unit, lash_kernel_value::sites::Unit::Library(function) if self.helpers.contains(function))
        {
            // Exact helper code keeps its coordinates; service-slot preflight
            // proved this operation-context transfer before any mutation.
            frame.link = self.successor;
            return Ok(());
        }
        let new = site(self.correspondence, &old, "frame statement", self.helpers)?;
        if let Some(pending) = &mut frame.pending {
            let suffix = |path: &[u32], held| -> Result<Vec<u32>, CarryFault> {
                let full = Site::new(
                    old.unit.clone(),
                    old.path
                        .iter()
                        .copied()
                        .chain(path.iter().copied())
                        .collect::<Vec<_>>(),
                );
                let to = site(self.correspondence, &full, held, self.helpers)?;
                if to.unit != new.unit || !to.path.starts_with(&new.path) {
                    return Err(state(format!("{held} {full} left its statement")).into());
                }
                Ok(to.path[new.path.len()..].to_vec())
            };
            pending.site = suffix(&pending.site, "pending action")?;
            match &mut pending.roles {
                RolesRecord::Flat(roles) => {
                    for role in std::sync::Arc::make_mut(roles) {
                        role.site = suffix(&role.site, "operand role")?;
                    }
                }
                RolesRecord::Spilled(chunks) => self.tree(chunks, |chunk| {
                    for role in &mut chunk.0 {
                        role.site = suffix(&role.site, "operand role")?;
                    }
                    Ok(())
                })?,
            }
        }
        frame.link = self.successor;
        frame.statement = SiteRef::new(&new, &self.access)?;
        // Locals are keyed by their declaration's Site and name. Carrying
        // the row alone would leave it under the predecessor function's key,
        // so the successor could not restore its saved value.
        let before = frame.bindings.clone();
        let mut bindings = Tree::new(false);
        self.visit(&before, |binding| {
            let mut binding = binding.clone();
            binding.declared = self.reference(&binding.declared, "variable declaration")?;
            let key = metadata_key(&(
                binding.declared.resolve(&self.access)?,
                binding.name.resolve(&self.access)?,
            ));
            if bindings.get(&key, &self.access)?.is_some() {
                return Err(state("carried variable declarations collide").into());
            }
            bindings.write(key, binding, &self.access)?;
            Ok(())
        })?;
        self.tree(&mut bindings, |_| Ok(()))?;
        frame.bindings = bindings;
        self.tree(&mut frame.loops, |row| {
            row.site = self.reference(&row.site, "loop")?;
            Ok(())
        })?;
        self.tree(&mut frame.finally, |row| {
            row.site = self.reference(&row.site, "cleanup")?;
            if let EnteredRecord::ReturnVia(held) = &mut row.entered {
                **held = self.reference(held, "return policy")?;
            }
            Ok(())
        })?;
        Ok(())
    }
}

/// Authenticated predecessor facts carried intact to the rewrite boundary.
/// The live selection is transient; the root remains its sole durable owner
/// of the executing helper-library identity.
#[derive(Clone, Debug)]
pub struct CarryContext {
    pub kernel: u32,
    pub closure: PageId,
    /// Ordinary defining code authenticated by the pinned closure's producer.
    pub code: std::collections::BTreeMap<
        lash_kernel_value::LinkId,
        std::collections::BTreeSet<lash_kernel_value::FunctionId>,
    >,
    pub links:
        std::collections::BTreeMap<lash_kernel_value::LinkId, lash_kernel_value::package::Link>,
    pub selection: lash_kernel_doc::Selection,
    /// Live function values keep their defining code and occurrence history.
    pub retained: std::collections::BTreeSet<lash_kernel_value::CallableId>,
    /// Producer-validated live/held operation uses; no resolution occurs here.
    pub uses: std::collections::BTreeMap<
        lash_kernel_value::OperationUse,
        lash_kernel_value::OperationUse,
    >,
}

/// Carries one authenticated foreign checkpoint. The caller must validate
/// the resulting image against the successor's executable before publishing it.
/// Closure selection belongs to the caller: a version carry supplies the
/// exact pinned closure's redeclarations; adoption selects a successor Link.
/// Values, entity ids, wait numbers, meters, limits and queue order are retained.
pub fn carry_checkpoint(
    source: &mut dyn PageSource,
    checkpoint: &Checkpoint,
    rewritten: &Rewritten,
    closure: PageId,
    successor: lash_kernel_value::LinkId,
    context: &CarryContext,
    helpers: &lash_kernel_doc::BuildHelpers,
) -> Result<(PageRef, Checkpoint), CarryFault> {
    let mut root = lash_kernel_state::kernel_internals::checkpoint_root(checkpoint).clone();
    root.helper_library = helpers.identity();
    let c = &rewritten.correspondence;
    if root.kernel != context.kernel {
        return Err(ParkedRefusal::Version {
            found: root.kernel,
            from: context.kernel,
        }
        .into());
    }
    if root.closure != context.closure {
        return Err(state(format!(
            "checkpoint closure {:?} differs from its pinned code {:?}",
            root.closure, context.closure
        ))
        .into());
    }
    if root.document != c.base {
        return Err(ParkedRefusal::Document {
            parked: root.document,
            rewritten: c.base,
        }
        .into());
    }
    if !matches!(root.status, RootStatus::Parked) {
        return Err(state("an ended image cannot be carried").into());
    }
    let carry = Carry {
        access: Access::borrowed(source),
        correspondence: c,
        predecessor: root.link,
        successor,
        helpers,
    };
    // Occurrence keys may be digests of long sites. Derive their rewrite from
    // the producer's correspondence, never decode or guess a digest's position.
    let keys: std::collections::BTreeMap<_, _> = c
        .entries()
        .iter()
        .map(|entry| (metadata_key(&entry.from), metadata_key(&entry.to)))
        .collect();
    let mut tasks = Tree::<TaskRecord>::of_keyed(&root.tasks);
    // Origins are validated without sealing even a private metadata page.
    carry.visit(&tasks, |row| {
        if let Some(origin) = &row.spawn_origin {
            carry.check_reference(origin, "spawn origin")?;
        }
        Ok(())
    })?;
    let retained_keys: std::collections::BTreeSet<_> = c
        .entries()
        .iter()
        .filter_map(|entry| {
            let lash_kernel_value::sites::Unit::Library(function) = &entry.from.unit else {
                return None;
            };
            (entry.from != entry.to
                && context.retained.contains(&lash_kernel_value::CallableId {
                    link: root.link,
                    function: *function,
                }))
            .then(|| metadata_key(&entry.from))
        })
        .collect();
    for rank in 0..tasks.len() {
        let (_, row) = tasks
            .rank_get(rank, &carry.access)?
            .ok_or_else(|| state("missing carry task"))?;
        let identity = row.identity.resolve(&carry.access)?;
        task(c, &identity, helpers)?;
        for occurrence_rank in 0..row.occurrences.len() {
            let (key, count) = row
                .occurrences
                .rank_get(occurrence_rank, &carry.access)?
                .ok_or_else(|| state("missing occurrence"))?;
            if matches!(count.owner, OccurrenceOwner::Authored) && !keys.contains_key(&key) {
                return Err(state("an action occurrence has no successor").into());
            }
        }
        for frame_rank in 0..row.frames.len() {
            let (_, frame) = row
                .frames
                .rank_get(frame_rank, &carry.access)?
                .ok_or_else(|| state("missing carry frame"))?;
            if !context.links.contains_key(&frame.link) {
                return Err(state("frame has no retained defining Link").into());
            }
            carry.check_frame(&frame)?;
            if frame.link == root.link {
                let at = frame.statement.resolve(&carry.access)?;
                if let lash_kernel_value::sites::Unit::Library(function) = &at.unit
                    && helpers.contains(function)
                {
                    check_helper_operation_context(
                        *function,
                        frame.link,
                        successor,
                        helpers,
                        &context.links,
                    )?;
                }
            }
        }
        if let Some(origin) = &row.spawn_origin {
            carry.check_reference(origin, "spawn origin")?;
        }
    }
    let session = Tree::<BindingRecord>::of_keyed(&root.session);
    carry.visit(&session, |binding| {
        carry.check_reference(&binding.declared, "session declaration")
    })?;
    let reads = Tree::<lash_kernel_value::OperationUse>::of_keyed(&root.reads);
    carry.visit(&reads, |use_| {
        if !context.uses.contains_key(use_) {
            return Err(state("read use has no successor correspondence").into());
        }
        Ok(())
    })?;
    carry.tree(&mut tasks, |row| {
        row.identity = carry.identity(row.identity)?;
        if let Some(origin) = &mut row.spawn_origin {
            *origin = carry.reference(origin, "spawn origin")?;
        }
        // Rekey from the old immutable tree so swaps cannot overwrite a row.
        let before = row.occurrences.clone();
        let mut after = Tree::new(false);
        for rank in 0..before.len() {
            let (key, count) = before
                .rank_get(rank, &carry.access)?
                .ok_or_else(|| state("missing occurrence"))?;
            let to = match count.owner {
                OccurrenceOwner::Authored => keys
                    .get(&key)
                    .ok_or_else(|| state("an action occurrence has no successor"))?,
                OccurrenceOwner::Helper(function) if helpers.contains(&function) => &key,
                // Compatibility proved this helper is not live before carry.
                OccurrenceOwner::Helper(_) => continue,
            };
            if after.get(to, &carry.access)?.is_some() {
                return Err(state("occurrences collide").into());
            }
            after.write(to.clone(), count, &carry.access)?;
        }
        // A rewritten activation and a captured predecessor function can both
        // run again. Preserve the latter's counter beside its mapped successor.
        for rank in 0..before.len() {
            let (key, count) = before
                .rank_get(rank, &carry.access)?
                .ok_or_else(|| state("missing retained occurrence"))?;
            if retained_keys.contains(&key) {
                if after.get(&key, &carry.access)?.is_some() {
                    return Err(state("retained and successor occurrences collide").into());
                }
                after.write(key, count, &carry.access)?;
            }
        }
        carry.tree(&mut after, |_| Ok(()))?;
        row.occurrences = after;
        carry.tree(&mut row.frames, |frame| carry.frame(frame))
    })?;
    let mut carried_reads = Tree::new(false);
    carry.visit(&reads, |use_| {
        let successor = context.uses[use_];
        let mut key = successor.link.as_bytes().to_vec();
        key.extend_from_slice(&lash_kernel_value::tokens::encode_body(&successor.slot));
        carried_reads.write(key, successor, &carry.access)?;
        if successor != *use_ && context.retained.iter().any(|held| held.link == use_.link) {
            let mut key = use_.link.as_bytes().to_vec();
            key.extend_from_slice(&lash_kernel_value::tokens::encode_body(&use_.slot));
            carried_reads.write(key, *use_, &carry.access)?;
        }
        Ok(())
    })?;
    carry.tree(&mut carried_reads, |_| Ok(()))?;
    root.reads = carried_reads.keyed_root()?;
    root.tasks = tasks.keyed_root()?;
    let mut session = Tree::<BindingRecord>::of_keyed(&root.session);
    carry.tree(&mut session, |binding| {
        binding.declared = carry.reference(&binding.declared, "session declaration")?;
        Ok(())
    })?;
    root.session = session.keyed_root()?;
    // Heap closures are function values, including unescaped locals. Their
    // defining callable and creation site remain in the retained predecessor.
    // Only active predecessor frames move to the successor executable.
    root.link = successor;
    root.document = c.result;
    root.kernel = rewritten.document.kernel();
    root.closure = closure;
    let children = [
        Some(root.heap),
        root.tasks.page(),
        root.ready.page(),
        root.waits.page(),
        root.joins.page(),
        root.withdrawn.page(),
        root.unreported.page(),
        root.reads.page(),
        root.session.page(),
        root.functions.page(),
    ]
    .into_iter()
    .flatten()
    .collect();
    let page = carry
        .access
        .seal(PageKind::CHECKPOINT_ROOT, encode_body(&root), children)?;
    Ok((page, lash_kernel_state::kernel_internals::checkpoint(root)))
}

/// The helper's exact validated static reach owns these declarations. An
/// equal library digest never authorizes rebinding an operation contract.
fn check_helper_operation_context(
    root: lash_kernel_value::FunctionId,
    predecessor: lash_kernel_value::LinkId,
    successor: lash_kernel_value::LinkId,
    helpers: &lash_kernel_doc::BuildHelpers,
    links: &std::collections::BTreeMap<lash_kernel_value::LinkId, lash_kernel_value::package::Link>,
) -> Result<(), ParkedRefusal> {
    use lash_kernel_value::package::Fill;
    let mut pending = vec![root];
    let mut seen = std::collections::BTreeSet::new();
    while let Some(function) = pending.pop() {
        if !seen.insert(function) {
            continue;
        }
        let registered = helpers
            .registered(&function)
            .ok_or(ParkedRefusal::HelperUnavailable {
                reason: lash_kernel_value::HelperUnavailableReason::Unknown,
                function,
            })?;
        for (slot, import) in registered.imports() {
            let key = lash_kernel_value::OperationSlot::Helper(
                lash_kernel_value::HelperOperationId::of_import(import),
            );
            let before = links
                .get(&predecessor)
                .and_then(|link| link.operation_fill(key));
            let after = links
                .get(&successor)
                .and_then(|link| link.operation_fill(key));
            if !matches!(before, Some(Fill::Operation(_) | Fill::Deferred { .. }))
                || before != after
            {
                return Err(ParkedRefusal::HelperOperationContext {
                    function,
                    slot: *slot,
                });
            }
        }
        pending.extend(helpers.callees(&function).into_iter().flatten().copied());
    }
    Ok(())
}
