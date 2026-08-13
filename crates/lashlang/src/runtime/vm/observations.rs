impl<'a, H: ExecutionHost> Vm<'a, H> {
    fn lashlang_execution_site_at(&self, instruction_ip: usize) -> Option<&LashlangExecutionSite> {
        self.chunk
            .lashlang_execution_sites
            .get(instruction_ip)
            .and_then(Option::as_ref)
    }

    fn begin_lashlang_execution(
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
        let occurrence = self
            .lashlang_execution_occurrences
            .entry(site.node_id.clone())
            .and_modify(|value| *value += 1)
            .or_insert(1);
        let occurrence = *occurrence;
        self.host
            .observe_lashlang_execution(LashlangExecutionObservation::NodeStarted {
                site: site.clone(),
                occurrence,
            });
        ActiveLashlangExecutionNode { site, occurrence }
    }

    pub(super) fn complete_lashlang_execution(&self, active: &ActiveLashlangExecutionNode) {
        self.host
            .observe_lashlang_execution(LashlangExecutionObservation::NodeCompleted {
                site: active.site.clone(),
                occurrence: active.occurrence,
            });
    }

    pub(super) fn fail_lashlang_execution(
        &self,
        active: &ActiveLashlangExecutionNode,
        error: impl Into<String>,
    ) {
        self.host
            .observe_lashlang_execution(LashlangExecutionObservation::NodeFailed {
                site: active.site.clone(),
                occurrence: active.occurrence,
                error: error.into(),
            });
    }

    pub(super) fn observe_child_started(
        &self,
        active: &ActiveLashlangExecutionNode,
        child: LashlangExecutionChild,
    ) {
        self.host
            .observe_lashlang_execution(LashlangExecutionObservation::ChildStarted {
                site: active.site.clone(),
                occurrence: active.occurrence,
                child,
            });
    }

    fn observe_branch_selection(
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
        let occurrence = self
            .lashlang_execution_occurrences
            .entry(site.node_id.clone())
            .and_modify(|value| *value += 1)
            .or_insert(1);
        let edge_id = match selected {
            ProcessBranchSelection::Then => branch.then_edge_id.clone(),
            ProcessBranchSelection::Else => branch.else_edge_id.clone(),
        };
        self.host
            .observe_lashlang_execution(LashlangExecutionObservation::BranchSelected {
                site,
                occurrence: *occurrence,
                edge_id,
                selected,
            });
    }
}
