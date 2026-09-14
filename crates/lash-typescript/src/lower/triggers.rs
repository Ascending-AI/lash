//! Trigger registration configs: the `inputs` arrow template.
//!
//! `inputs: (event) => ({ tick: event })` is a *template*, not a callback.
//! The arrow is erased here: its parameter becomes the unresolved
//! `trigger.event` path the linker rewrites into the `$lash.trigger.event` IR
//! marker, and every other value lowers as an ordinary expression in the
//! enclosing scope — evaluated once, at registration, exactly as the record
//! form it replaces did. Nothing of the arrow reaches canonical IR: no
//! `FunctionExpr`, no closure allocation, no temporary binding, no parameter
//! type. An artifact built from the arrow form is therefore byte-identical to
//! one built from the retired `inputs: { tick: trigger.event }` record, which
//! is what `trigger_inputs_arrow_matches_the_retired_record_form` proves.
//!
//! The config's *shape* is checked before any of its values lower. A model
//! that writes `inputs: { event }` has an unbound `event` inside the literal;
//! lowering first would report that binding instead of the shape that is
//! actually wrong, and the shape is the thing it has to change.

use super::*;

/// The trigger resource operations whose config carries `inputs`.
///
/// `registerTrigger` is the convenience spelling of `triggers.register`, and
/// `update`/`revive` take the same registration record, so retiring the global
/// for one of them and not the others would strand the other two.
pub(super) fn is_trigger_registration_operation(operation: &str) -> bool {
    matches!(operation, "register" | "update" | "revive")
}

/// The rewrite every trigger-input diagnostic points at.
const INPUTS_ARROW_REWRITE: &str = "pass the fired event as the `inputs` arrow's parameter: `inputs: (event) => ({ tick: event })`, where `tick` is the target's parameter name; omit `inputs` entirely when the target takes exactly one parameter";

/// The unresolved path the linker recognises as "the whole fired event".
fn trigger_event_marker() -> LashExpr {
    LashExpr::ResourceRef(ResourceRefExpr::unresolved(vec![
        "trigger".into(),
        "event".into(),
    ]))
}

fn mentions_identifier(expr: &Expr, name: &str) -> bool {
    if matches!(expr, Expr::Ident(found, _) if found == name) {
        return true;
    }
    expr.children()
        .any(|child| mentions_identifier(child, name))
}

/// Whether `expr` names the same descriptor as the registration's `source`.
///
/// Identifiers and module paths only: those are the spellings a source can
/// have and still be namable twice in one config, and they are the ones
/// GitHub #1350 reports.
fn names_same_descriptor(expr: &Expr, source: &Expr) -> bool {
    match (expr, source) {
        (Expr::Ident(left, _), Expr::Ident(right, _)) => left == right,
        _ => match (module_path(expr), module_path(source)) {
            (Some(left), Some(right)) => left == right,
            _ => false,
        },
    }
}

impl Lowerer {
    /// Lowers a trigger registration config, erasing the `inputs` arrow.
    ///
    /// A config that is not a static object literal is lowered as an ordinary
    /// expression and refused by the linker, which is what happened before the
    /// arrow existed: there is no shape here to check.
    pub(super) fn lower_trigger_config(&mut self, config: &Expr) -> Result<LashExpr, Diagnostic> {
        let Expr::Object(properties) = config else {
            return self.lower_expr(config);
        };
        let Some(entries) = properties
            .iter()
            .map(|property| match property {
                ObjectProperty::KeyValue(PropertyKey::Static(name), value) => {
                    Some((name.as_str(), value))
                }
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
        else {
            return self.lower_expr(config);
        };
        let source = entries
            .iter()
            .find_map(|(name, value)| (*name == "source").then_some(*value));
        if let Some(target) = entries
            .iter()
            .find_map(|(name, value)| (*name == "target").then_some(*value))
        {
            self.require_literal_process_target(target)?;
        }
        for (_, value) in &entries {
            self.reject_trigger_event_spellings(value, source)?;
        }
        let mut lowered = Vec::with_capacity(entries.len());
        for (name, value) in entries {
            let value = if name == "inputs" {
                self.lower_trigger_input_template(value)?
            } else if name == "target" {
                self.lower_process_target(value)?
            } else {
                self.lower_expr(value)?
            };
            lowered.push((name.into(), value));
        }
        Ok(LashExpr::Record(lowered))
    }

    /// Lowers a registration target to the process it names.
    ///
    /// `require_literal_process_target` has already established that the target
    /// is a top-level `defineProcess` binding, and such a binding holds exactly
    /// this reference — so naming the process directly is the same value the
    /// variable read would have produced. It is not the same *program*: a
    /// variable read is a capture, and `defineProcess.run` refuses captures, so
    /// reading the binding made a process registering a trigger against
    /// another process unwritable (FIG-3059). A reference has nothing to
    /// capture.
    ///
    /// Only a registration written *inside* a process body takes this route.
    /// At the top level the binding is in scope with nothing to capture, and
    /// the variable read is the form every stored artifact was built from; the
    /// canonical IR there is durable, so it does not move for a rewrite that
    /// buys nothing.
    fn lower_process_target(&mut self, target: &Expr) -> Result<LashExpr, Diagnostic> {
        if self.process_depth > 0
            && let Expr::Ident(name, _) = target
            && let Ok(BindingRole::ProcessDefinition(process)) =
                self.binding(name).map(|binding| binding.role.clone())
        {
            return Ok(LashExpr::ProcessRef {
                process: process.as_str().into(),
            });
        }
        self.lower_expr(target)
    }

    /// The registration target names a top-level `defineProcess` binding.
    ///
    /// `start` has always required that (`ProcessTargetStaticRequired`); the
    /// registration path did not, so `const t = p` followed by
    /// `registerTrigger({ target: t, ... })` linked, and the runtime derived a
    /// subscription key from an alias no reader of the registration can see.
    /// The prompt has said "Literal target" the whole time; this is the rule
    /// that makes it true, on all four registration spellings.
    fn require_literal_process_target(&self, target: &Expr) -> Result<(), Diagnostic> {
        if let Expr::Ident(name, _) = target
            && matches!(
                self.binding(name).map(|binding| &binding.role),
                Ok(BindingRole::ProcessDefinition(_))
            )
        {
            return Ok(());
        }
        Err(Diagnostic::new(
            DiagnosticCode::ProcessTargetStaticRequired,
            "a trigger target is the name of a top-level defineProcess binding, never an alias or an expression",
            None,
        ))
    }

    /// The two spellings a model reaches for that no longer exist.
    fn reject_trigger_event_spellings(
        &self,
        expr: &Expr,
        source: Option<&Expr>,
    ) -> Result<(), Diagnostic> {
        if let Expr::Member {
            object,
            property: MemberProperty::Field(field),
            ..
        } = expr
            && field == "event"
        {
            if let Some(source) = source
                && names_same_descriptor(object.as_ref(), source)
            {
                return Err(Diagnostic::with_repair(
                    DiagnosticCode::TriggerSourceEventAccess,
                    "a trigger source descriptor is opaque and has no `event` property: one source can feed many registrations, so the fired event does not belong to it",
                    INPUTS_ARROW_REWRITE,
                    None,
                ));
            }
            if matches!(object.as_ref(), Expr::Ident(root, _) if root == "trigger")
                && !self.has_binding("trigger")
            {
                return Err(retired_trigger_event_diagnostic());
            }
        }
        for child in expr.children() {
            self.reject_trigger_event_spellings(child, source)?;
        }
        Ok(())
    }

    /// Erases `(event) => ({ tick: event })` into the record the linker reads.
    fn lower_trigger_input_template(&mut self, inputs: &Expr) -> Result<LashExpr, Diagnostic> {
        let Expr::Function(function) = inputs else {
            return Err(inputs_shape_diagnostic(
                "`inputs` must be an arrow literal that names the fired event",
            ));
        };
        if function.is_async {
            return Err(inputs_shape_diagnostic(
                "the `inputs` arrow is a template the compiler erases, so it cannot be `async`",
            ));
        }
        let [Pattern::Ident(parameter, _)] = function.params.as_slice() else {
            return Err(inputs_shape_diagnostic(
                "the `inputs` arrow takes exactly one plain parameter, the fired event",
            ));
        };
        // A parameter that shadows an enclosing binding makes every other
        // value ambiguous to a reader: `label: event` would be the fired event
        // and not the `event` declared three lines above it.
        if self.has_binding(parameter) {
            return Err(inputs_shape_diagnostic(format!(
                "the `inputs` arrow's parameter `{parameter}` shadows a binding of the same name, so a fixed value could not name that binding: rename the parameter"
            )));
        }
        let FunctionBody::Expression(body) = &function.body else {
            return Err(inputs_shape_diagnostic(
                "the `inputs` arrow has no body to run: it returns one object expression",
            ));
        };
        let Expr::Object(properties) = body.as_ref() else {
            return Err(inputs_shape_diagnostic(
                "the `inputs` arrow returns an object literal of target parameters",
            ));
        };
        let mut seen = BTreeSet::new();
        let mut lowered = Vec::with_capacity(properties.len());
        for property in properties {
            let ObjectProperty::KeyValue(PropertyKey::Static(key), value) = property else {
                return Err(inputs_shape_diagnostic(
                    "every `inputs` key is a target parameter name, so computed keys, spreads and methods have nothing to name",
                ));
            };
            if !seen.insert(key.clone()) {
                return Err(inputs_shape_diagnostic(format!(
                    "`inputs` maps `{key}` twice"
                )));
            }
            if matches!(value, Expr::Ident(found, _) if found == parameter) {
                lowered.push((key.as_str().into(), trigger_event_marker()));
                continue;
            }
            if mentions_identifier(value, parameter) {
                return Err(inputs_shape_diagnostic(format!(
                    "`{parameter}` is the whole fired event and is substituted at fire time, so it can only stand alone as a property value, never be projected, nested, called or captured"
                )));
            }
            lowered.push((key.as_str().into(), self.lower_expr(value)?));
        }
        Ok(LashExpr::Record(lowered))
    }
}

/// Whether a member access spells the retired `trigger.event` global.
///
/// Scoped to the bare identifier: a program that binds `trigger` itself reads
/// its own value, here as everywhere else. The caller checks the binding.
pub(super) fn names_the_retired_trigger_event(object: &Expr, property: &MemberProperty) -> bool {
    matches!(property, MemberProperty::Field(field) if field == "event")
        && matches!(object, Expr::Ident(root, _) if root == "trigger")
}

pub(super) fn retired_trigger_event_diagnostic() -> Diagnostic {
    Diagnostic::with_repair(
        DiagnosticCode::TriggerEventRemoved,
        "`trigger` is bound nowhere in a TypeScript program and `trigger.event` is no longer part of the dialect",
        INPUTS_ARROW_REWRITE,
        None,
    )
}

fn inputs_shape_diagnostic(message: impl Into<String>) -> Diagnostic {
    Diagnostic::with_repair(
        DiagnosticCode::TriggerInputsLiteralRequired,
        message,
        INPUTS_ARROW_REWRITE,
        None,
    )
}
