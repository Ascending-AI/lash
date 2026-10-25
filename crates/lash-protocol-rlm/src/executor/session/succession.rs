//! Kernel-format succession precedes normal FIG-6926 cell-start relinking.
use super::*;
use lash_execution_contracts::process::linking::ReachSelector;
use lash_vm_runtime::KernelMigrationRefusal;

impl CodeModeExecutionState {
    /// Preflight every saved definition before staging any replacement session.
    /// A format successor initially has the predecessor's fills; the next cell
    /// fulfils the migrated document against its own current package set.
    pub(crate) async fn succeed_saved_functions(
        &mut self,
        claim: Option<&lash_core::ReferrerClaim>,
    ) -> Result<(), SessionError> {
        if self.kernel.writes().previous().is_none() {
            return Ok(());
        }
        let Some(session) = self.current.session else {
            return Ok(());
        };
        let metas = self
            .pages
            .read(move |source| SessionEditor::open(Some(&session), source)?.functions())
            .await
            .map_err(protocol)?;
        let state = |message: String| {
            SessionOpenError::Succession(KernelMigrationRefusal::State { message })
        };
        let held = self
            .kernel
            .library()
            .map_err(SessionOpenError::Succession)?;
        let mut approved = BTreeMap::new();
        let mut edits = Vec::new();
        let mut publications = Vec::new();
        let mut closures = Vec::new();
        for (name, meta) in metas {
            let mut function = self.lower_function(&name, &meta).map_err(state)?;
            if function.document.kernel() == self.kernel.writes().number() {
                approved.insert(name, function);
                continue;
            }
            let plan = self
                .kernel
                .successor(&function.document)
                .map_err(SessionOpenError::Succession)?;
            let refuse = |reason: String| {
                SessionOpenError::Succession(KernelMigrationRefusal::NotAdmitted {
                    document: function.document.id(),
                    reason,
                })
            };
            if plan.to != self.kernel.writes() {
                return Err(refuse("saved function successor has the wrong kernel".into()).into());
            }
            let documents = self
                .documents
                .as_ref()
                .ok_or_else(|| state("session succession has no document owner".into()))?;
            let mut content = documents
                .load_linked(function.link)
                .await
                .map_err(|error| state(error.to_string()))?;
            let old = content
                .links
                .get(&function.link)
                .filter(|old| old.document == function.document.id())
                .ok_or_else(|| refuse("saved function differs from its defining Link".into()))?;
            let document =
                lash_kernel_doc::IdentifiedDocument::new(plan.rewritten.document.clone());
            let next = lash_vocabulary::package::Link {
                document: document.id(),
                fills: old.fills.clone(),
            };
            let link = next.identity();
            content
                .documents
                .insert(document.id(), Arc::clone(document.shared()));
            content.links.insert(link, next);
            let inspection = documents
                .inspect_link(function.link, &ReachSelector::AllExports)
                .await
                .map_err(|error| state(error.to_string()))?;
            // The same operation-use proof as process format succession. This
            // proves the representation rewrite, not the next cell's packages.
            lash_vm_runtime::plan_adoption::<()>(
                &content,
                function.link,
                link,
                &inspection.operations.keys().copied().collect(),
                &BTreeMap::new(),
                &BTreeMap::new(),
            )
            .map_err(|error| refuse(error.to_string()))?;
            let reach = documents
                .inspect(
                    &content,
                    link,
                    &lash_kernel_package::ReachSelection::AllExports,
                )
                .map_err(|error| refuse(error.to_string()))?;
            let loaded = lash_vm_broker::kernel::loader::declare_reach(
                &document,
                &content
                    .documents
                    .iter()
                    .map(|(id, doc)| (*id, Arc::clone(doc)))
                    .collect(),
                &reach.functions,
                &reach.machine,
                &held,
                documents.helpers().as_ref(),
                &lash_kernel_doc::Selection::Fresh,
            )
            .map_err(|error| refuse(error.to_string()))?;
            let id = plan
                .rewritten
                .functions
                .get(&function.callable.function)
                .copied()
                .ok_or_else(|| refuse("saved callable has no successor identity".into()))?;
            publications.push((function.link, document.clone(), link));
            function.document = document;
            function.link = link;
            function.callable = lash_vocabulary::CallableId { link, function: id };
            edits.push((
                name.clone(),
                function.description(),
                PageRef {
                    id: PageId(loaded.closure.reference().0),
                    grant: PageGrant::NONE,
                },
                function.callable,
                plan.rewritten.correspondence,
            ));
            closures.push(loaded.closure);
            approved.insert(name, function);
        }
        if edits.is_empty() && self.current.kernel == self.kernel.writes().number() {
            return Ok(());
        }
        let mut spool = self.turn_spool().map_err(protocol)?;
        // Read and prove every heap closure site on a private COW session before
        // publishing code or advancing the descriptor. A refusal changes no root.
        let carried = self
            .pages
            .read(move |source| {
                let mut editor = SessionEditor::open(Some(&session), source)?;
                for (name, description, closure, callable, proof) in &edits {
                    editor.succeed_function(name, description, *closure, *callable, proof)?;
                }
                editor.seal()
            })
            .await
            .map_err(|error| state(error.to_string()))?;
        if !publications.is_empty() {
            let claim = claim
                .ok_or_else(|| state("saved function succession has no frame claim".into()))?;
            let documents = self
                .documents
                .as_ref()
                .ok_or_else(|| state("session succession has no document owner".into()))?;
            for (before, document, expected) in publications {
                let actual = documents
                    .hold_format_successor(claim, before, &document)
                    .await
                    .map_err(|error| state(error.to_string()))?;
                if actual != expected {
                    return Err(state(
                        "saved function successor Link differs from its proof".into(),
                    )
                    .into());
                }
            }
        }
        for closure in closures {
            for node in closure.nodes().values() {
                let node = lash_core_store::checkpoint_graph::node::VerifiedNode::check(
                    node.clone(),
                    &lash_core_store::checkpoint_graph::DecodeLimits::within_total(u64::MAX),
                )
                .map_err(|error| state(error.to_string()))?;
                spool
                    .seal(node, &|_| true)
                    .map_err(|error| state(error.to_string()))?;
            }
        }
        let mut next = self.current.clone();
        next.kernel = self.kernel.writes().number();
        next.session = Some(carried);
        next.pins = approved
            .values()
            .flat_map(|f| f.document.functions.keys().copied())
            .collect();
        self.current = self.seal_descriptor(next, spool).map_err(protocol)?;
        self.view.functions = Some(approved);
        self.final_owed = true;
        Ok(())
    }
}
