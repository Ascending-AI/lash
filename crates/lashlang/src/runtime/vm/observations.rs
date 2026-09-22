use super::*;
use crate::{LashlangExecutionCallSite, LashlangExecutionFailure};

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

    /// Aggregates are counted in the same map as execution sites, under a key
    /// no site can mint: node ids are `{kind}:{24 hex}` and this key is not.
    /// That map is what rides the VM continuation, so the count survives a park
    /// and a process segment handover, which a counter held beside the VM would
    /// not — two identical aggregates straddling a snapshot would both be the
    /// first (ADR 0065).
    ///
    /// The instruction is the key rather than an execution site because an
    /// aggregate in a function body has no site at all: `lashlang_execution_paths`
    /// walks `program.main`, so nothing inside a declaration is described. An
    /// instruction pointer is defined for every aggregate, and it is stable for
    /// a given compiled program — the guarantee `BYTECODE_FORMAT_VERSION`
    /// exists to make, and the one a restored continuation's own `ip` already
    /// depends on.
    pub(super) fn next_aggregate_occurrence(&mut self, instruction_ip: usize) -> u64 {
        let occurrence = self
            .lashlang_execution_occurrences
            .entry(format!("aggregate@{instruction_ip}"))
            .and_modify(|value| *value += 1)
            .or_insert(1);
        *occurrence
    }

    pub(super) fn begin_lashlang_execution_site(
        &mut self,
        site: LashlangExecutionSite,
    ) -> ActiveLashlangExecutionNode {
        let occurrence = next_occurrence(&mut self.lashlang_execution_occurrences, &site.node_id);
        self.host
            .observe_lashlang_execution(LashlangExecutionObservation::NodeStarted {
                site: site.clone(),
                occurrence,
            });
        ActiveLashlangExecutionNode { site, occurrence }
    }

    pub(super) fn observe_lashlang_execution_step(&mut self, instruction_ip: usize) {
        let Some(site) = self
            .chunk
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
        self.host
            .observe_lashlang_execution(LashlangExecutionObservation::NodeStarted {
                site: site.clone(),
                occurrence,
            });
        self.host
            .observe_lashlang_execution(LashlangExecutionObservation::NodeCompleted {
                site: site.clone(),
                occurrence,
            });
    }

    pub(super) fn complete_lashlang_execution(&self, active: &ActiveLashlangExecutionNode) {
        let _ = self
            .host
            .take_lashlang_effect_failure(&LashlangExecutionCallSite {
                site: active.site.clone(),
                occurrence: active.occurrence,
            });
        self.host
            .observe_lashlang_execution(LashlangExecutionObservation::NodeCompleted {
                site: active.site.clone(),
                occurrence: active.occurrence,
            });
    }

    pub(super) fn fail_lashlang_execution(
        &self,
        active: &ActiveLashlangExecutionNode,
        code: impl Into<String>,
        message: impl Into<String>,
    ) {
        let call_site = LashlangExecutionCallSite {
            site: active.site.clone(),
            occurrence: active.occurrence,
        };
        let failure = self
            .host
            .take_lashlang_effect_failure(&call_site)
            .map(LashlangExecutionFailure::Effect)
            .unwrap_or_else(|| LashlangExecutionFailure::Runtime {
                code: code.into(),
                message: message.into(),
            });
        self.host
            .observe_lashlang_execution(LashlangExecutionObservation::NodeFailed {
                site: active.site.clone(),
                occurrence: active.occurrence,
                failure,
            });
    }

    /// A batch shape or VM validation failure is not the failure of any one
    /// dispatched leaf. Drop pending leaf facts before emitting this terminal.
    pub(super) fn fail_lashlang_execution_runtime(
        &self,
        active: &ActiveLashlangExecutionNode,
        error: &RuntimeError,
    ) {
        let _ = self
            .host
            .take_lashlang_effect_failure(&LashlangExecutionCallSite {
                site: active.site.clone(),
                occurrence: active.occurrence,
            });
        self.host
            .observe_lashlang_execution(LashlangExecutionObservation::NodeFailed {
                site: active.site.clone(),
                occurrence: active.occurrence,
                failure: LashlangExecutionFailure::Runtime {
                    code: error.code().to_owned(),
                    message: error.to_string(),
                },
            });
    }

    pub(super) fn observe_branch_selection(
        &mut self,
        instruction_ip: usize,
        selected: ProcessBranchSelection,
    ) {
        let Some(site) = self.lashlang_execution_site_at(instruction_ip).cloned() else {
            return;
        };
        let Some(branch) = site.branch.as_ref() else {
            return;
        };
        let occurrence = next_occurrence(&mut self.lashlang_execution_occurrences, &site.node_id);
        let edge_id = match selected {
            ProcessBranchSelection::Then => branch.then_edge_id.clone(),
            ProcessBranchSelection::Else => branch.else_edge_id.clone(),
        };
        self.host
            .observe_lashlang_execution(LashlangExecutionObservation::BranchSelected {
                site,
                occurrence,
                edge_id,
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
