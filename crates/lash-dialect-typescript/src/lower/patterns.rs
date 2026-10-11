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
                let iterator = self.invoke("ts.array.iterator", &[value], Ty::Unknown)?;
                let next = self.invoke(
                    "ts.get",
                    &[iterator.clone(), Operand::text("next")],
                    Ty::Unknown,
                )?;
                let next = self.invoke("ts.callable", &[next], Ty::Unknown)?;
                let exhausted = self.let_expr(Operand::bool(false).expr(), Ty::Bool);
                for element in elements {
                    let item = self.iterator_value(&iterator, &next, &exhausted)?;
                    if let Some(element) = element {
                        self.destructure(element, item, mode)?;
                    }
                }
                if let Some(rest) = rest {
                    let tail = self.let_expr(Expr::List(Vec::new()), Ty::Unknown);
                    let body = self.block(|this| {
                        let item = this.iterator_value(&iterator, &next, &exhausted)?;
                        let onward = this.native("bool.not", vec![exhausted.expr()])?;
                        this.exit_unless(onward);
                        let index = this.native("list.len", vec![tail.expr()])?;
                        this.store(
                            Place::Member(lash_kernel_doc::Member::Index {
                                target: tail.expr(),
                                index,
                            }),
                            item,
                        );
                        Ok(())
                    })?;
                    self.emit_while(Operand::bool(true).expr(), body);
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
                            Key::Computed(self.invoke("ts.to_property_key", &[key], Ty::Text)?)
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

    /// IteratorStepValue retains exhaustion across defaults and elisions.
    fn iterator_value(
        &mut self,
        iterator: &Operand,
        next: &Operand,
        exhausted: &Operand,
    ) -> Lowering<Operand> {
        let value = self.let_expr(Operand::undefined().expr(), Ty::Unknown);
        let step = self.block(|this| {
            let args = this.let_expr(Expr::List(Vec::new()), Ty::Unknown);
            let step = this.emit_action(
                lash_kernel_doc::Action::Call {
                    callee: lash_kernel_doc::Callee::Value(super::statements::variable_of(next)),
                    args: vec![iterator.atom.clone(), args.atom],
                },
                Ty::Unknown,
            );
            let done = this.invoke(
                "ts.get",
                &[step.clone(), Operand::text("done")],
                Ty::Unknown,
            )?;
            let done = this.invoke("ts.to_boolean", &[done], Ty::Bool)?;
            this.store(
                Place::Variable(super::statements::variable_of(exhausted)),
                done,
            );
            let read = this.block(|this| {
                let item = this.invoke("ts.get", &[step, Operand::text("value")], Ty::Unknown)?;
                this.store(
                    Place::Variable(super::statements::variable_of(&value)),
                    item,
                );
                Ok(())
            })?;
            this.emit_if(exhausted.expr(), Buf::default(), read);
            Ok(())
        })?;
        self.emit_if(exhausted.expr(), Buf::default(), step);
        Ok(value)
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
