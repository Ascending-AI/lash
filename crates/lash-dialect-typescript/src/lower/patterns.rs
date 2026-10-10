//! Destructuring, in binding and in assignment position.

use lash_kernel_doc::{Expr, Literal, Place};

use super::calls::Key;
use super::{BindingKind, Buf, Lowerer, Lowering, Operand, Ty};
use crate::adapter as ast;

/// What a pattern's names are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    /// Bindings their scope already declared: `var`, `let`, `const`.
    Declared,
    /// Bindings declared here and live at once: a parameter, a catch
    /// binding, a loop's `let`.
    Local,
    /// Existing variables and properties.
    Assign,
}

impl Lowerer<'_> {
    pub(super) fn destructure(
        &mut self,
        pattern: &ast::Pattern,
        value: Operand,
        mode: Mode,
    ) -> Lowering<()> {
        match pattern {
            ast::Pattern::Ident(name, _) => {
                match mode {
                    Mode::Declared => self.initialise(name, value),
                    Mode::Local => {
                        let kernel = self.declare(name, BindingKind::Local);
                        self.bind(kernel, value);
                    }
                    Mode::Assign => {
                        let kernel = self.resolve_for_write(name, self.span)?;
                        self.store(Place::Variable(kernel), value);
                    }
                }
                Ok(())
            }
            ast::Pattern::Rest(inner) => self.destructure(inner, value, mode),
            ast::Pattern::Member { object, property } => {
                let value = self.pin(value);
                let object = self.lower_expr(object)?;
                let object = self.pin(object);
                let key = self.lower_key(property)?;
                self.set_member(&object, &key, value)
            }
            ast::Pattern::Assign { target, default } => {
                let name = match target.as_ref() {
                    ast::Pattern::Ident(name, _) => Some(name.as_str()),
                    _ => None,
                };
                let value = self.or_default(value, default, name)?;
                self.destructure(target, value, mode)
            }
            ast::Pattern::Array { elements, rest } => {
                let items = self.invoke("ts.iterate", &[value], Ty::Unknown)?;
                #[expect(clippy::cast_precision_loss, reason = "a pattern's element count")]
                let count = Operand::number(elements.len() as f64);
                if elements.iter().any(Option::is_some) {
                    let padded =
                        self.invoke("ts.pad", &[items.clone(), count.clone()], Ty::Unknown)?;
                    for (index, element) in elements.iter().enumerate() {
                        let Some(element) = element else { continue };
                        let item = self.let_expr(Self::element(&padded, index), Ty::Unknown);
                        let item = self.invoke("ts.hole_value", &[item], Ty::Unknown)?;
                        self.destructure(element, item, mode)?;
                    }
                }
                if let Some(rest) = rest {
                    let tail = self.invoke("ts.rest", &[items, count], Ty::Unknown)?;
                    self.destructure(rest, tail, mode)?;
                }
                Ok(())
            }
            ast::Pattern::Object { properties, rest } => {
                let value = self.pin(value);
                let checked = self.invoke(
                    "ts.require_object_coercible",
                    std::slice::from_ref(&value),
                    Ty::Unknown,
                )?;
                self.discard(checked);
                let mut taken = Vec::new();
                for property in properties {
                    let key = match &property.key {
                        ast::PropertyKey::Static(name) => Key::Static(name.clone()),
                        ast::PropertyKey::Computed(key) => {
                            let key = self.lower_expr(key)?;
                            Key::Computed(self.pin(key))
                        }
                    };
                    taken.push(key.operand().expr());
                    let member = self.get_member(&value, &key)?;
                    self.destructure(&property.value, member, mode)?;
                }
                if let Some(rest) = rest {
                    let taken = self.let_expr(Expr::List(taken), Ty::Unknown);
                    let others = self.invoke("ts.object_rest", &[value, taken], Ty::Unknown)?;
                    self.destructure(rest, others, mode)?;
                }
                Ok(())
            }
        }
    }

    /// `value`, or the default's value when `value` is `undefined`. The
    /// default is evaluated only then.
    pub(super) fn or_default(
        &mut self,
        value: Operand,
        default: &ast::Expr,
        name: Option<&str>,
    ) -> Lowering<Operand> {
        let slot = self.temp();
        self.bind(slot.clone(), value);
        let missing = self.same(Expr::Variable(slot.clone()), Expr::Literal(Literal::Absent))?;
        let fill = self.block(|this| {
            let default = match name {
                Some(name) => this.named_expression(default, name)?,
                None => this.lower_expr(default)?,
            };
            this.store(Place::Variable(slot.clone()), default);
            Ok(())
        })?;
        self.emit_if(missing, fill, Buf::default());
        Ok(Operand::variable(slot, Ty::Unknown))
    }
}
