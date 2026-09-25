use super::*;
use crate::LashlangExecutionFailure;

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

    fn lashlang_execution_site_at(&self, instruction_ip: usize) -> Option<&LashlangExecutionSite> {
        self.chunk
            .lashlang_execution_sites
            .get(instruction_ip)
            .and_then(Option::as_ref)
    }

    pub(super) fn begin_lashlang_execution(
        &mut self,
        instruction_ip: usize,
    ) -> Option<ActiveLashlangExecutionNode> {
        let site = self.lashlang_execution_site_at(instruction_ip)?.clone();
        Some(self.begin_lashlang_execution_site(site))
    }

    pub(super) fn begin_lashlang_execution_site(
        &mut self,
        site: LashlangExecutionSite,
    ) -> ActiveLashlangExecutionNode {
        let occurrence = next_occurrence(&mut self.lashlang_execution_occurrences, &site.node_id);
        self.observe(|| LashlangExecutionObservation::NodeStarted {
            site: site.clone(),
            occurrence,
        });
        ActiveLashlangExecutionNode { site, occurrence }
    }

    /// Starts the observed node of a call instruction, when the host observes
    /// execution. Unobserved, the call still takes its occurrence, so the
    /// numbering is the same either way, but no site is copied.
    pub(super) fn begin_lashlang_call(
        &mut self,
        instruction_ip: usize,
    ) -> Option<ActiveLashlangExecutionNode> {
        if self.host.observes_lashlang_execution() {
            return self.begin_lashlang_execution(instruction_ip);
        }
        let chunk = self.chunk;
        let site = chunk
            .lashlang_execution_sites
            .get(instruction_ip)?
            .as_ref()?;
        next_occurrence(&mut self.lashlang_execution_occurrences, &site.node_id);
        None
    }

    pub(super) fn observe_lashlang_execution_step(&mut self, instruction_ip: usize) {
        let chunk = self.chunk;
        let Some(site) = chunk
            .lashlang_execution_sites
            .get(instruction_ip)
            .and_then(Option::as_ref)
        else {
            return;
        };
        let occurrence = next_occurrence(
            &mut self.lashlang_execution_occurrences,
            site.node_id.as_str(),
        );
        self.observe(|| LashlangExecutionObservation::NodeStarted {
            site: site.clone(),
            occurrence,
        });
        self.observe(|| LashlangExecutionObservation::NodeCompleted {
            site: site.clone(),
            occurrence,
        });
    }

    /// Hands the host an observation it builds only when the host observes.
    pub(super) fn observe(&self, observation: impl FnOnce() -> LashlangExecutionObservation) {
        if self.host.observes_lashlang_execution() {
            self.host.observe_lashlang_execution(observation());
        }
    }

    pub(super) fn complete_lashlang_execution(&self, active: &ActiveLashlangExecutionNode) {
        self.observe(|| LashlangExecutionObservation::NodeCompleted {
            site: active.site.clone(),
            occurrence: active.occurrence,
        });
    }

    pub(super) fn fail_lashlang_execution(
        &self,
        active: &ActiveLashlangExecutionNode,
        error: &RuntimeError,
    ) {
        let failure = error
            .execution_host_error()
            .and_then(|source| source.tool_failure())
            .map(LashlangExecutionFailure::Effect)
            .unwrap_or_else(|| LashlangExecutionFailure::Runtime {
                code: error.code().to_owned(),
                message: error.to_string(),
            });
        self.emit_lashlang_execution_failure(active, failure);
    }

    pub(super) fn emit_lashlang_execution_failure(
        &self,
        active: &ActiveLashlangExecutionNode,
        failure: LashlangExecutionFailure,
    ) {
        self.observe(|| LashlangExecutionObservation::NodeFailed {
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
            .lashlang_execution_sites
            .get(instruction_ip)
            .and_then(Option::as_ref)
        else {
            return;
        };
        let Some(branch) = site.branch.as_ref() else {
            return;
        };
        let occurrence = next_occurrence(&mut self.lashlang_execution_occurrences, &site.node_id);
        self.observe(|| LashlangExecutionObservation::BranchSelected {
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

fn next_occurrence(occurrences: &mut FxHashMap<String, u64>, node_id: &str) -> u64 {
    if let Some(occurrence) = occurrences.get_mut(node_id) {
        *occurrence += 1;
        return *occurrence;
    }
    occurrences.insert(node_id.to_owned(), 1);
    1
}
