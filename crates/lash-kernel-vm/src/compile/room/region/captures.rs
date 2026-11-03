//! A borrowed cell root costs zero; each read owns its scalar value holding.
use super::*;

impl Source {
    pub(in crate::compile::room::region) fn captured_input(
        &self,
        slots: &OwnerSlots,
        at: &Point,
    ) -> Option<Slot> {
        if at.site.unit != self.site.unit || !at.site.path.starts_with(&self.site.path) {
            return None;
        }
        let mut node = doc::Node::Block(&self.body);
        for index in &at.site.path[self.site.path.len()..] {
            node = *node.children().get(*index as usize)?;
        }
        let doc::Node::Expr(doc::Expr::Variable(name)) = node else {
            return None;
        };
        let owner = root_owners::visible(&slots.scopes, &at.site, name)?;
        match slots.root(owner, TaskId::MAIN, 0)? {
            logical::Owner::Value {
                slot: slot @ Slot::Capture(_),
                ..
            } => Some(slot),
            _ => None,
        }
    }
}

impl Compiler<'_> {
    pub(super) fn capture(&mut self, slot: Slot) -> Live {
        if let Some(captured) = self.captures.iter().find(|captured| captured.slot == slot) {
            return captured.live;
        }
        let live = self.fresh();
        self.captures.push(Entry { slot, live });
        live
    }
}
