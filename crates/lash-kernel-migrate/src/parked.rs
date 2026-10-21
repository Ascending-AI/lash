//! Copy-on-write coordinate carry. Unchanged data trees retain their roots;
//! rewritten records are never published until the embedding owner commits.
use crate::{ParkedRefusal, Rewritten};
use lash_kernel_doc::Correspondence;
use lash_kernel_doc::heap::{HeapFault, PageId, PageRef};
use lash_kernel_doc::kernel_internals::{EffectIdentity, TaskIdentity, effect_id, effect_parts};
use lash_kernel_state::kernel_internals::{
    directory::{Body, ObjectHead},
    metadata::metadata_key,
    tree::{Access, Cell, SealWork, Tree},
};
use lash_kernel_state::paged::*;
use lash_vocabulary::sites::EffectId;
use lash_vocabulary::sites::Site;
use lash_vocabulary::tokens::encode_body;

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
fn site(c: &Correspondence, from: &Site, held: &str) -> Result<Site, ParkedRefusal> {
    c.successor(from)
        .cloned()
        .ok_or_else(|| ParkedRefusal::SiteNotCarried {
            site: from.clone(),
            held: held.into(),
        })
}
fn task(c: &Correspondence, from: &TaskIdentity) -> Result<TaskIdentity, ParkedRefusal> {
    // Walk lineage iteratively: user fan-out may be deeper than the Rust stack.
    let mut steps = Vec::new();
    let mut at = from;
    while let TaskIdentity::Spawned(spawn) = at {
        steps.push((site(c, &spawn.site, "task spawn")?, spawn.occurrence));
        at = &spawn.parent;
    }
    let mut to = TaskIdentity::Main;
    for (site, occurrence) in steps.into_iter().rev() {
        to = TaskIdentity::Spawned(lash_kernel_doc::kernel_internals::SpawnIdentity {
            parent: std::sync::Arc::new(to),
            site,
            occurrence,
        });
    }
    Ok(to)
}
/// Rekeys a wait without changing its occurrence, loops, or recorded request.
pub fn carry_effect(c: &Correspondence, from: &EffectId) -> Result<EffectId, ParkedRefusal> {
    let from = effect_parts(from);
    Ok(effect_id(EffectIdentity {
        task: task(c, &from.task)?,
        site: site(c, &from.site, "wait")?,
        occurrence: from.occurrence,
        loops: from
            .loops
            .iter()
            .map(|held| {
                Ok(lash_kernel_doc::kernel_internals::LoopIteration {
                    site: site(c, &held.site, "wait loop")?,
                    iteration: held.iteration,
                })
            })
            .collect::<Result<_, ParkedRefusal>>()?,
    }))
}
struct Carry<'a, 's> {
    access: Access<'s>,
    correspondence: &'a Correspondence,
}
impl Carry<'_, '_> {
    fn reference(&self, from: &SiteRef, held: &str) -> Result<SiteRef, CarryFault> {
        let from = from.resolve(&self.access)?;
        Ok(SiteRef::new(
            &site(self.correspondence, &from, held)?,
            &self.access,
        )?)
    }
    fn identity(&self, from: TaskIdentityRef) -> Result<TaskIdentityRef, CarryFault> {
        let resolved = from.resolve(&self.access)?;
        let carried = task(self.correspondence, &resolved)?;
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
    fn frame(&self, frame: &mut FrameRecord) -> Result<(), CarryFault> {
        let old = frame.statement.resolve(&self.access)?;
        let new = site(self.correspondence, &old, "frame statement")?;
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
                let to = site(self.correspondence, &full, held)?;
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
        frame.statement = SiteRef::new(&new, &self.access)?;
        self.tree(&mut frame.bindings, |binding| {
            binding.declared = self.reference(&binding.declared, "variable declaration")?;
            Ok(())
        })?;
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

/// Carries one authenticated foreign checkpoint. The caller must validate
/// the resulting image against the successor's executable before publishing it.
/// Closure selection belongs to the caller: a version carry supplies the
/// exact pinned closure's redeclarations; only explicit adoption selects a set.
/// Values, entity ids, wait numbers, meters, limits and queue order are retained.
pub fn carry_checkpoint(
    source: &mut dyn PageSource,
    checkpoint: &Checkpoint,
    rewritten: &Rewritten,
    closure: PageId,
    pinned: (u32, PageId),
) -> Result<(PageRef, Checkpoint), CarryFault> {
    let mut root = lash_kernel_state::kernel_internals::checkpoint_root(checkpoint).clone();
    let c = &rewritten.correspondence;
    if root.kernel != pinned.0 {
        return Err(ParkedRefusal::Version {
            found: root.kernel,
            from: pinned.0,
        }
        .into());
    }
    if root.closure != pinned.1 {
        return Err(state(format!(
            "checkpoint closure {:?} differs from its pinned code {:?}",
            root.closure, pinned.1
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
    };
    let mut tasks = Tree::<TaskRecord>::of_keyed(&root.tasks);
    for rank in 0..tasks.len() {
        let (_, row) = tasks
            .rank_get(rank, &carry.access)?
            .ok_or_else(|| state("missing carry task"))?;
        if let Some(origin) = &row.spawn_origin {
            carry.reference(origin, "spawn origin")?;
        }
    }
    // Occurrence keys may be digests of long sites. Derive their rewrite from
    // the producer's correspondence, never decode or guess a digest's position.
    let keys: std::collections::BTreeMap<_, _> = c
        .entries()
        .iter()
        .map(|entry| (metadata_key(&entry.from), metadata_key(&entry.to)))
        .collect();
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
            let to = keys
                .get(&key)
                .ok_or_else(|| state("an action occurrence has no successor"))?;
            if after.get(to, &carry.access)?.is_some() {
                return Err(state("occurrences collide").into());
            }
            after.write(to.clone(), count, &carry.access)?;
        }
        carry.tree(&mut after, |_| Ok(()))?;
        row.occurrences = after;
        carry.tree(&mut row.frames, |frame| carry.frame(frame))
    })?;
    root.tasks = tasks.keyed_root()?;
    let mut session = Tree::<BindingRecord>::of_keyed(&root.session);
    carry.tree(&mut session, |binding| {
        binding.declared = carry.reference(&binding.declared, "session declaration")?;
        Ok(())
    })?;
    root.session = session.keyed_root()?;
    let answer = carry.access.read(root.heap)?;
    if answer.kind != PageKind::KEYED_LEAF {
        return Err(state("not a heap root").into());
    }
    let mut heap = lash_kernel_state::kernel_internals::heap_impl::header_root(root.heap, &answer)?;
    for tree in [&mut heap.directory, &mut heap.families] {
        if let Some(page) = tree.page_mut() {
            *page = *answer
                .children
                .iter()
                .find(|child| child.id == page.id)
                .ok_or_else(|| state("undeclared heap child"))?;
        }
    }
    let mut directory = Tree::<ObjectHead>::of_keyed(&heap.directory);
    carry.tree(&mut directory, |head| {
        if let Body::Closure { site, .. } = &mut head.body {
            *site = carry.reference(site, "closure")?;
        }
        Ok(())
    })?;
    heap.directory = directory.keyed_root()?;
    let mut tokens = lash_vocabulary::tokens::TokenWriter::new();
    tokens.sum(0, 1);
    use lash_vocabulary::tokens::TokenCodec as _;
    heap.encode(&mut tokens);
    root.heap = carry.access.seal(
        PageKind::KEYED_LEAF,
        tokens.into_bytes(),
        [heap.directory.page(), heap.families.page()]
            .into_iter()
            .flatten()
            .collect(),
    )?;
    root.document = c.result;
    root.kernel = rewritten.document.manifest.kernel;
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
