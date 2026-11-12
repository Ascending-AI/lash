//! The failed wait behind an error value, followed by object identity.

use lash_kernel_doc::Value;

use crate::WaitId;

#[derive(Default)]
pub(crate) struct Catches {
    pub(crate) raised: Vec<(usize, WaitId, Value)>,
}

impl Catches {
    pub(crate) fn wait_raised(&mut self, task: usize, wait: WaitId, error: Value) {
        self.raised.push((task, wait, error));
    }

    pub(crate) fn roots(&self, visit: &mut dyn FnMut(&Value)) {
        for (_, _, error) in &self.raised {
            visit(error);
        }
    }
}
