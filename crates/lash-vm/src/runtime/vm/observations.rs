use std::num::NonZeroU64;

use super::*;
use crate::LashVmExecutionFailure;

impl<'a, H: ExecutionHost> Vm<'a, H> {
    pub(super) fn record_instruction_profile(
        &mut self,
        tag: InstructionProfileTag,
        elapsed_ns: u128,
    ) {
        let Some(profile) = &mut self.profile else {
            return;
        };
        let index = tag as usize;
        profile.instruction_counts[index] += 1;
        profile.instruction_times[index] += elapsed_ns;
    }

    pub(super) fn record_builtin_profile(&mut self, builtin: IntrinsicOp, elapsed_ns: u128) {
        let Some(profile) = &mut self.profile else {
            return;
        };
        let index = builtin.profile_tag() as usize;
        profile.builtin_counts[index] += 1;
        profile.builtin_times[index] += elapsed_ns;
    }

    pub(crate) fn take_profile(&mut self) -> ProfileReport {
        let Some(profile) = self.profile.take() else {
            return ProfileReport::default();
        };
        profile.finish()
    }

    fn lash_vm_execution_site_at(&self, instruction_ip: usize) -> Option<&LashVmExecutionSite> {
        self.chunk
            .lash_vm_execution_sites
            .get(instruction_ip)
            .and_then(Option::as_ref)
    }

    pub(super) fn begin_lash_vm_execution(
        &mut self,
        instruction_ip: usize,
    ) -> Option<LashVmExecutionCallSite> {
        let site = self.lash_vm_execution_site_at(instruction_ip)?.clone();
        Some(self.begin_lash_vm_execution_site(site))
    }

    pub(super) fn begin_lash_vm_effect(
        &mut self,
        instruction_ip: usize,
        reissued: bool,
    ) -> Option<LashVmExecutionCallSite> {
        if !reissued {
            return self.begin_lash_vm_execution(instruction_ip);
        }
        let site = self.lash_vm_execution_site_at(instruction_ip)?.clone();
        Some(self.reissue_lash_vm_execution_site(site))
    }

    /// The node of an operation a run issues again after it parked on it:
    /// it takes back the occurrence its park gave up, and is not started a
    /// second time. The loop context is the one it parked in: a parked run
    /// advances no loop.
    pub(super) fn reissue_lash_vm_execution_site(
        &mut self,
        site: LashVmExecutionSite,
    ) -> LashVmExecutionCallSite {
        let occurrence = next_occurrence(&mut self.lash_vm_execution_occurrences, &site);
        call_site(site, occurrence, self.loop_context())
    }

    /// The occurrence a handle minted by the instruction at `instruction_ip`
    /// is of that instruction's site, in the loops the run is inside now.
    /// Nothing starts here: the operation's node starts when the handle is
    /// awaited, under this occurrence.
    pub(super) fn mint_pending_occurrence(
        &mut self,
        instruction_ip: usize,
    ) -> Option<lash_sansio::WorkflowOccurrence> {
        let chunk = self.chunk;
        let site = chunk
            .lash_vm_execution_sites
            .get(instruction_ip)?
            .as_ref()?;
        let occurrence = next_occurrence(&mut self.lash_vm_execution_occurrences, site);
        Some(lash_sansio::WorkflowOccurrence {
            site: site.site.clone(),
            occurrence,
            loops: self.loop_context(),
        })
    }

    /// The node of an operation awaited through its handle: the occurrence
    /// the handle was minted as, wherever the run is now. It starts once,
    /// when it is first issued; issued again after a park, it is the same
    /// node.
    pub(super) fn begin_minted_lash_vm_execution_site(
        &self,
        site: LashVmExecutionSite,
        minted: &lash_sansio::WorkflowOccurrence,
        reissued: bool,
    ) -> LashVmExecutionCallSite {
        let active = LashVmExecutionCallSite {
            at: minted.clone(),
            kind: site.kind,
            label: site.label,
        };
        if !reissued {
            self.observe_fact(&active, || LashVmExecutionFact::NodeStarted);
        }
        active
    }

    /// Takes back the occurrence `active` began with: the run parked on the
    /// operation its node issues, and issues it again when it resumes. Never
    /// called for an occurrence a pending handle holds: the handle is live
    /// again and still names it.
    pub(super) fn rewind_lash_vm_execution(&mut self, active: &LashVmExecutionCallSite) {
        rewind_occurrence(&mut self.lash_vm_execution_occurrences, &active.at);
    }

    pub(super) fn begin_lash_vm_execution_site(
        &mut self,
        site: LashVmExecutionSite,
    ) -> LashVmExecutionCallSite {
        let occurrence = next_occurrence(&mut self.lash_vm_execution_occurrences, &site);
        let active = call_site(site, occurrence, self.loop_context());
        self.observe_fact(&active, || LashVmExecutionFact::NodeStarted);
        active
    }

    /// Starts the observed node of a call instruction, when the host observes
    /// execution. Unobserved, the call still takes its occurrence, so the
    /// numbering is the same either way, but no site is copied.
    pub(super) fn begin_lash_vm_call(
        &mut self,
        instruction_ip: usize,
    ) -> Option<LashVmExecutionCallSite> {
        if self.host.observes_lash_vm_execution() {
            return self.begin_lash_vm_execution(instruction_ip);
        }
        let chunk = self.chunk;
        let site = chunk
            .lash_vm_execution_sites
            .get(instruction_ip)?
            .as_ref()?;
        next_occurrence(&mut self.lash_vm_execution_occurrences, site);
        None
    }

    pub(super) fn observe_lash_vm_execution_step(&mut self, instruction_ip: usize) {
        let chunk = self.chunk;
        let Some(site) = chunk
            .lash_vm_execution_sites
            .get(instruction_ip)
            .and_then(Option::as_ref)
        else {
            return;
        };
        let occurrence = next_occurrence(&mut self.lash_vm_execution_occurrences, site);
        if !self.host.observes_lash_vm_execution() {
            return;
        }
        let active = call_site(site.clone(), occurrence, self.loop_context());
        self.observe_fact(&active, || LashVmExecutionFact::NodeStarted);
        self.observe_fact(&active, || LashVmExecutionFact::NodeCompleted);
    }

    /// Keeps the loop context at one point of the loop whose site
    /// `instruction_ip` carries. A loop with no site is not tracked.
    pub(super) fn mark_loop(&mut self, instruction_ip: usize, mark: LoopMark) {
        let chunk = self.chunk;
        let Some(site) = chunk
            .lash_vm_execution_sites
            .get(instruction_ip)
            .and_then(Option::as_ref)
        else {
            return;
        };
        match mark {
            LoopMark::Enter => {
                self.loop_activations += 1;
                self.loop_stack.push(ActiveLoop {
                    site: site.site.clone(),
                    activation: self.loop_activations,
                    checks: 0,
                    iterations: 0,
                    checking: false,
                    call_depth: self.frames.len(),
                    handler_depth: self.handlers.len(),
                });
            }
            LoopMark::Check => {
                if let Some(active) = self.active_loop(site) {
                    active.checks += 1;
                    active.checking = true;
                }
            }
            LoopMark::Iteration => {
                if let Some(active) = self.active_loop(site) {
                    active.iterations += 1;
                    active.checking = false;
                }
                self.observe_lash_vm_execution_step(instruction_ip);
            }
            LoopMark::Exit => {
                if self.active_loop(site).is_some() {
                    self.loop_stack.pop();
                }
            }
        }
    }

    /// The innermost loop, when it is the loop at `site` in the active frame.
    fn active_loop(&mut self, site: &LashVmExecutionSite) -> Option<&mut ActiveLoop> {
        let call_depth = self.frames.len();
        self.loop_stack
            .last_mut()
            .filter(|active| active.call_depth == call_depth && site.site == active.site)
    }

    /// Leaves every loop a return or a caught throw has left: those entered
    /// under more call frames or exception handlers than are live now.
    pub(super) fn unwind_loops(&mut self) {
        let (call_depth, handler_depth) = (self.frames.len(), self.handlers.len());
        while self.loop_stack.last().is_some_and(|active| {
            active.call_depth > call_depth || active.handler_depth > handler_depth
        }) {
            self.loop_stack.pop();
        }
    }

    /// The loops the run is inside, outermost first, as an occurrence
    /// beginning now records them.
    pub(super) fn loop_context(&self) -> Vec<lash_sansio::WorkflowLoopFrame> {
        self.loop_stack
            .iter()
            .map(|active| lash_sansio::WorkflowLoopFrame {
                site: active.site.clone(),
                activation: active.activation,
                position: if active.checking {
                    lash_sansio::WorkflowLoopPosition::Check(active.checks)
                } else {
                    lash_sansio::WorkflowLoopPosition::Body(active.iterations)
                },
            })
            .collect()
    }

    /// Hands the host an observation it builds only when the host observes.
    pub(super) fn observe(&self, observation: impl FnOnce() -> LashVmExecutionObservation) {
        if self.host.observes_lash_vm_execution() {
            self.host.observe_lash_vm_execution(observation());
        }
    }

    /// Hands the host one fact about the occurrence `active` names.
    pub(super) fn observe_fact(
        &self,
        active: &LashVmExecutionCallSite,
        fact: impl FnOnce() -> LashVmExecutionFact,
    ) {
        self.observe(|| LashVmExecutionObservation {
            call_site: active.clone(),
            fact: fact(),
        });
    }

    pub(super) fn complete_lash_vm_execution(&self, active: &LashVmExecutionCallSite) {
        self.observe_fact(active, || LashVmExecutionFact::NodeCompleted);
    }

    pub(super) fn fail_lash_vm_execution(
        &self,
        active: &LashVmExecutionCallSite,
        error: &RuntimeError,
    ) {
        let failure = error
            .execution_host_error()
            .and_then(|source| source.tool_failure())
            .map(LashVmExecutionFailure::Effect)
            .unwrap_or_else(|| LashVmExecutionFailure::Runtime {
                code: error.code().to_owned(),
                message: error.to_string(),
            });
        self.emit_lash_vm_execution_failure(active, failure);
    }

    pub(super) fn emit_lash_vm_execution_failure(
        &self,
        active: &LashVmExecutionCallSite,
        failure: LashVmExecutionFailure,
    ) {
        self.observe_fact(active, || LashVmExecutionFact::NodeFailed { failure });
    }

    pub(super) fn observe_branch_selection(
        &mut self,
        instruction_ip: usize,
        selected: ProcessBranchSelection,
    ) {
        let chunk = self.chunk;
        let Some(site) = chunk
            .lash_vm_execution_sites
            .get(instruction_ip)
            .and_then(Option::as_ref)
        else {
            return;
        };
        if site.kind != lash_sansio::ExecutionNodeKind::Branch {
            return;
        }
        let occurrence = next_occurrence(&mut self.lash_vm_execution_occurrences, site);
        self.observe(|| LashVmExecutionObservation {
            call_site: call_site(site.clone(), occurrence, self.loop_context()),
            fact: LashVmExecutionFact::BranchSelected { selected },
        });
    }
}

/// The occurrence `occurrence` of `site`, begun inside `loops`.
fn call_site(
    site: LashVmExecutionSite,
    occurrence: NonZeroU64,
    loops: Vec<lash_sansio::WorkflowLoopFrame>,
) -> LashVmExecutionCallSite {
    LashVmExecutionCallSite {
        at: lash_sansio::WorkflowOccurrence {
            site: site.site,
            occurrence,
            loops,
        },
        kind: site.kind,
        label: site.label,
    }
}

/// Takes back the occurrence a site began with, when the run parked on the
/// operation the site issues (FIG-4159): the resumed run begins it again,
/// under the same occurrence an unparked run gives it.
fn rewind_occurrence(occurrences: &mut SiteOccurrences, at: &lash_sansio::WorkflowOccurrence) {
    let site = &at.site;
    let Some(sites) = occurrences.get_mut(&site.node_id) else {
        return;
    };
    match NonZeroU64::new(at.occurrence.get() - 1) {
        None => {
            sites.retain(|(path, _)| *path != site.site_path);
            if sites.is_empty() {
                occurrences.remove(&site.node_id);
            }
        }
        Some(previous) => {
            if let Some((_, count)) = sites.iter_mut().find(|(path, _)| *path == site.site_path) {
                *count = previous;
            }
        }
    }
}

/// The next occurrence of `site`, counted per site: two sites of one node
/// each count from 1.
fn next_occurrence(occurrences: &mut SiteOccurrences, site: &LashVmExecutionSite) -> NonZeroU64 {
    let site = &site.site;
    if let Some(sites) = occurrences.get_mut(&site.node_id) {
        if let Some((_, count)) = sites.iter_mut().find(|(path, _)| *path == site.site_path) {
            *count = count.saturating_add(1);
            return *count;
        }
        sites.push((site.site_path.clone(), NonZeroU64::MIN));
        return NonZeroU64::MIN;
    }
    occurrences.insert(
        site.node_id.clone(),
        vec![(site.site_path.clone(), NonZeroU64::MIN)],
    );
    NonZeroU64::MIN
}
