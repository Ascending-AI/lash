//! Constructor lowering and its proven collection result kinds.
use super::{Lowerer, Lowering, Operand, Ty};
use crate::{Diagnostic, DiagnosticCode, adapter as ast};

impl Lowerer<'_> {
    pub(super) fn lower_new(
        &mut self,
        constructor: &str,
        args: &[ast::CallArg],
    ) -> Lowering<Operand> {
        if let Some((root, _)) = constructor.split_once('.')
            && !self.is_bound(root)
            && self.namespaces.is_root(root)
        {
            return Err(Diagnostic::with_repair(
                DiagnosticCode::NewUnsupported,
                format!(
                    "`new {constructor}` is not in the TypeScript dialect: a package value is built by its type's static"
                ),
                format!(
                    "call the static the prompt lists under `{constructor}`, such as `await {constructor}.create()`"
                ),
                self.span,
            ));
        }
        let function = (!self.is_bound(constructor))
            .then(|| self.table.constructors.get(constructor).copied())
            .flatten();
        let Some(function) = function else {
            let Some(idiom) = DiagnosticCode::NewUnsupported.accepted_idiom() else {
                unreachable!("constructors name their accepted forms");
            };
            return Err(Diagnostic::with_repair(
                DiagnosticCode::NewUnsupported,
                format!("`new {constructor}` is not in the TypeScript dialect"),
                format!("use a plain-object factory for `{constructor}`; {}", idiom),
                self.span,
            ));
        };
        let args = self.arguments(args)?;
        let ty = match constructor {
            "Array" => Ty::Array,
            "Map" => Ty::Map,
            "Set" => Ty::Set,
            _ => Ty::Unknown,
        };
        self.invoke(function, &[Operand::undefined(), args], ty)
    }
}
