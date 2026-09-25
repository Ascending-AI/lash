//! The `collection_transform` role's TypeScript spelling: a method call whose
//! receiver, callback and operands the role's shape names.

use lashlang::{Expr, StructuralRole};

use super::{Printer, TypeScriptSourceError};

impl<'p> Printer<'p> {
    /// A collection-transform role prints back as `receiver.<operation>(fn)`
    /// when it binds no operand beyond its receiver and callback; any other
    /// setup (an initial value, extra arguments) has no one-call spelling
    /// here.
    pub(super) fn collection_transform(
        &self,
        expression: &Expr,
    ) -> Result<Option<String>, TypeScriptSourceError> {
        let Expr::Role {
            role: StructuralRole::CollectionTransform { operation },
            expr,
        } = expression
        else {
            return Ok(None);
        };
        let Some(parts) = lashlang::CollectionTransformParts::of(expr) else {
            return Ok(None);
        };
        if !parts.operands.is_empty() {
            return Err(TypeScriptSourceError::Unrepresentable {
                kind: "a collection transform with extra arguments",
            });
        }
        Ok(Some(format!(
            "{}.{}({})",
            self.member_target(parts.receiver)?,
            self.identifier("operation", operation.as_str())?,
            self.expression(parts.callback)?
        )))
    }
}
