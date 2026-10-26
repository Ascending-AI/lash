//! Engine identity construction and decomposition for kernel crates only.
use crate::sites::EffectIdentity;
use crate::sites::TaskIdentity;
pub fn effect_id(raw: EffectIdentity) -> crate::sites::EffectId {
    crate::sites::EffectId(raw)
}
pub fn effect_parts(id: &crate::sites::EffectId) -> &EffectIdentity {
    &id.0
}
pub fn task_label(raw: TaskIdentity) -> crate::sites::TaskLabel {
    crate::sites::TaskLabel(raw)
}
pub fn task_parts(id: &crate::sites::TaskLabel) -> &TaskIdentity {
    &id.0
}

pub fn execution_path(
    rows: Vec<(crate::sites::TaskIdentity, Vec<u32>)>,
) -> crate::observation::ExecutionPath {
    crate::observation::ExecutionPath(
        rows.into_iter()
            .map(|(task, starts)| (crate::sites::TaskLabel(task), starts))
            .collect(),
    )
}
