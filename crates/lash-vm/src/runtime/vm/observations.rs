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
    ) -> Option<ActiveLashVmExecutionNode> {
        let site = self.lash_vm_execution_site_at(instruction_ip)?.clone();
        Some(self.begin_lash_vm_execution_site(site))
    }

    pub(super) fn begin_lash_vm_effect(
        &mut self,
        instruction_ip: usize,
        reissued: bool,
    ) -> Option<ActiveLashVmExecutionNode> {
        if !reissued {
            return self.begin_lash_vm_execution(instruction_ip);
        }
        let site = self.lash_vm_execution_site_at(instruction_ip)?.clone();
        Some(self.reissue_lash_vm_execution_site(site))
    }

    /// The node of an operation a run issues again after it parked on it:
    /// it takes back the occurrence its park gave up, and is not started a
    /// second time.
    pub(super) fn reissue_lash_vm_execution_site(
        &mut self,
        site: LashVmExecutionSite,
    ) -> ActiveLashVmExecutionNode {
        let occurrence = next_occurrence(&mut self.lash_vm_execution_occurrences, &site.node_id);
        ActiveLashVmExecutionNode { site, occurrence }
    }

    /// Takes back the occurrence `active` began with: the run parked on the
    /// operation its node issues, and issues it again when it resumes.
    pub(super) fn rewind_lash_vm_execution(&mut self, active: &ActiveLashVmExecutionNode) {
        rewind_occurrence(
            &mut self.lash_vm_execution_occurrences,
            &active.site.node_id,
            active.occurrence,
        );
    }

    pub(super) fn begin_lash_vm_execution_site(
        &mut self,
        site: LashVmExecutionSite,
    ) -> ActiveLashVmExecutionNode {
        let occurrence = next_occurrence(&mut self.lash_vm_execution_occurrences, &site.node_id);
        self.observe(|| LashVmExecutionObservation::NodeStarted {
            site: site.clone(),
            occurrence,
        });
        ActiveLashVmExecutionNode { site, occurrence }
    }

    /// Starts the observed node of a call instruction, when the host observes
    /// execution. Unobserved, the call still takes its occurrence, so the
    /// numbering is the same either way, but no site is copied.
    pub(super) fn begin_lash_vm_call(
        &mut self,
        instruction_ip: usize,
    ) -> Option<ActiveLashVmExecutionNode> {
        if self.host.observes_lash_vm_execution() {
            return self.begin_lash_vm_execution(instruction_ip);
        }
        let chunk = self.chunk;
        let site = chunk
            .lash_vm_execution_sites
            .get(instruction_ip)?
            .as_ref()?;
        next_occurrence(&mut self.lash_vm_execution_occurrences, &site.node_id);
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
        let occurrence = next_occurrence(
            &mut self.lash_vm_execution_occurrences,
            site.node_id.as_str(),
        );
        self.observe(|| LashVmExecutionObservation::NodeStarted {
            site: site.clone(),
            occurrence,
        });
        self.observe(|| LashVmExecutionObservation::NodeCompleted {
            site: site.clone(),
            occurrence,
        });
    }

    /// Hands the host an observation it builds only when the host observes.
    pub(super) fn observe(&self, observation: impl FnOnce() -> LashVmExecutionObservation) {
        if self.host.observes_lash_vm_execution() {
            self.host.observe_lash_vm_execution(observation());
        }
    }

    pub(super) fn complete_lash_vm_execution(&self, active: &ActiveLashVmExecutionNode) {
        self.observe(|| LashVmExecutionObservation::NodeCompleted {
            site: active.site.clone(),
            occurrence: active.occurrence,
        });
    }

    pub(super) fn fail_lash_vm_execution(
        &self,
        active: &ActiveLashVmExecutionNode,
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
        active: &ActiveLashVmExecutionNode,
        failure: LashVmExecutionFailure,
    ) {
        self.observe(|| LashVmExecutionObservation::NodeFailed {
            site: active.site.clone(),
            occurrence: active.occurrence,
            failure,
        });
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
        let Some(branch) = site.branch.as_ref() else {
            return;
        };
        let occurrence = next_occurrence(&mut self.lash_vm_execution_occurrences, &site.node_id);
        self.observe(|| LashVmExecutionObservation::BranchSelected {
            site: site.clone(),
            occurrence,
            edge_id: match selected {
                ProcessBranchSelection::Then => branch.then_edge_id.clone(),
                ProcessBranchSelection::Else => branch.else_edge_id.clone(),
            },
            selected,
        });
    }
}

/// Takes back the occurrence a node began with, when the run parked on the
/// operation the node issues (FIG-4159): the resumed run begins it again,
/// under the same occurrence an unparked run gives it.
fn rewind_occurrence(occurrences: &mut FxHashMap<String, u64>, node_id: &str, occurrence: u64) {
    if occurrence <= 1 {
        occurrences.remove(node_id);
    } else {
        occurrences.insert(node_id.to_owned(), occurrence - 1);
    }
}

fn next_occurrence(occurrences: &mut FxHashMap<String, u64>, node_id: &str) -> u64 {
    if let Some(occurrence) = occurrences.get_mut(node_id) {
        *occurrence += 1;
        return *occurrence;
    }
    occurrences.insert(node_id.to_owned(), 1);
    1
}
