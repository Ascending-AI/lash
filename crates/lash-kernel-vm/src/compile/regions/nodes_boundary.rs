//! Conservative public-boundary guards for compiled helper calls.
use super::{LibId, Libs, Node};
use lash_kernel_doc::Value;
use lash_vocabulary::Type;

pub(super) fn typed(libs: &Libs<'_>, lib: LibId) -> bool {
    libs.exe
        .lib(lib)
        .header
        .signature
        .params
        .iter()
        .any(|param| param.ty != Type::Any)
}

/// Unsupported shapes and cold payloads miss before the helper executes;
/// the reference performs the full public check and reports its typed error.
pub(super) fn arguments(libs: &Libs<'_>, lib: LibId, nodes: Vec<Node>) -> Option<Vec<Node>> {
    let params = &libs.exe.lib(lib).header.signature.params;
    if params.len() != nodes.len() {
        return None;
    }
    params
        .iter()
        .zip(nodes)
        .map(|(param, node)| {
            let fits = crate::compile::import_result_check(&param.ty)?;
            let optional = param.optional;
            let warms = param.ty == Type::Text;
            Some(Box::new(
                move |slots: &[crate::machine::SlotState], cx: &mut super::Cx<'_>| {
                    let value = node(slots, cx)?;
                    if warms && cx.arena.any_cold() {
                        return None;
                    }
                    (fits(value) || (optional && matches!(value, Value::Absent))).then_some(value)
                },
            ) as Node)
        })
        .collect()
}

pub(super) fn result(libs: &Libs<'_>, lib: LibId, node: Node) -> Node {
    let ty = libs.exe.lib(lib).header.signature.result.clone();
    Box::new(move |slots, cx| {
        // A public check warms reachable payloads. Decline before a helper
        // allocates when that boundary could read cold data.
        if ty == Type::Text && cx.arena.any_cold() {
            return None;
        }
        node(slots, cx).filter(|value| crate::compile::import_result_fits(&ty, *value))
    })
}
