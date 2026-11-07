//! Root actions create their canonical atom owners before native completion.
use super::*;

impl Source {
    pub(in crate::compile::room) fn root_action(
        &self,
        at: &Point,
    ) -> Result<&doc::Action, Refused> {
        if at.site.unit != self.site.unit || !at.site.path.starts_with(&self.site.path) {
            return Err(Refused::Source);
        }
        let mut node = doc::Node::Block(&self.body);
        for index in &at.site.path[self.site.path.len()..] {
            node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
        }
        match node {
            doc::Node::Action(action @ doc::Action::Call { .. }) => Ok(action),
            _ => Err(Refused::Source),
        }
    }

    pub(crate) fn native_inputs(&self, at: &Point) -> Result<Box<[Owner]>, Refused> {
        if let Ok(action) = self.root_action(at) {
            return Ok((0..super::super::super::action_atoms(action).len())
                .map(|index| Owner::Atom {
                    at: at.site.clone(),
                    index: index as u32,
                })
                .collect());
        }
        self.compile_event(at, ExitKind::Normal)?
            .events()
            .next()
            .map(|event| event.inputs.clone())
            .ok_or(Refused::Path)
    }
}

impl ExactEvent {
    pub(super) fn check_root_native(
        &self,
        arena: &Arena,
        roots: &ExactRoots<'_>,
        facts: &ExternalFacts<'_>,
    ) -> Result<(), Refused> {
        let [(_, ExactValue::Prepared(proof))] = roots.outputs.as_slice() else {
            return Err(Refused::Facts);
        };
        let contract = facts.native.ok_or(Refused::Facts)?;
        if self.at.phase != Phase::Commit
            || !crate::compile::scalar(&proof.value())
            || !proof.graph().is_current(arena)
            || !facts.entities.is_empty()
            || !facts.scratch.is_empty()
            || !contract.events.is_empty()
        {
            return Err(Refused::Facts);
        }
        let canonical = doc::NativeResourceContract::returned(
            doc::Args::new(facts.arguments, arena.objects(), &arena.semantics()),
            &proof.value(),
        )
        .map_err(|_| Refused::Facts)?;
        if contract != &canonical {
            return Err(Refused::Facts);
        }
        Ok(())
    }
}
