//! A function signature's TypeScript spelling.
//!
//! The lowerer gives a destructured, defaulted, cell-boxed or
//! `arguments`-reading function's parameters generated slots and binds the
//! authored names in a prologue ahead of the body: `x = cell_new(slot)` for
//! a boxed parameter, `t = ArrayFromIterable(slot)` and element reads for an
//! array pattern, the object pattern's copy, coercibility check and key
//! reads, and `t = slot; name = (t === undefined ? default : t)` for a
//! default. The prologue is `lower_pattern`'s output, recursively: every
//! element, property and default target is a pattern in turn. The authored
//! signature — `([k, v = d], {x}, ...rest) => ..` — is what the prologue
//! re-lowers to, so the printer consumes the prologue and spells the
//! pattern.

use std::borrow::Cow;
use std::collections::BTreeMap;

use lashlang::{CoercingBinaryOp, Expr, FunctionExpr, OperandLogicalOp, StructuralRole};

use super::{
    LOWERED_BINDING_PREFIX, Printed, Printer, TypeScriptSourceError, is_typescript_identifier, key,
    stdlib_call,
};

/// What `signature` recovered: the parameter spellings and the body after
/// the prologue they consumed.
pub(super) struct Signature<'a> {
    pub params: Vec<String>,
    /// The body once the prologue is consumed: borrowed when nothing was
    /// consumed, rebuilt around the remainder when the prologue sat inside
    /// the body block's completion list.
    pub body: Cow<'a, Expr>,
    /// The names the parameters bind, for the body's `bound` list: the
    /// function's own slots and every name a recovered pattern declares.
    pub bound: Vec<String>,
    /// The internal slot `arguments` reads resolve to, when the function
    /// snapshots its argv: its reads display as `arguments` for the body's
    /// print. `None` when the function binds no arguments object.
    pub arguments: Option<String>,
    /// The leading parameters that are neither defaulted nor rest — the
    /// arity a `__lashlang_closure` wrap records.
    pub required: usize,
}

/// A pattern's spelling and how much of the prologue it consumed.
struct PatternMatch<'a> {
    spelling: String,
    /// The items the pattern's bindings occupy.
    used: usize,
    /// The default the incoming `Choice` source carried, handed to the level
    /// that spells `= default`. `None` for any other source.
    carried: Option<&'a Expr>,
    /// The pattern is `target = default`: a parameter it spells is not a
    /// required one.
    defaulted: bool,
}

/// The expression `lower_pattern` bound a pattern's input to — where in the
/// IR the value it destructures reads from.
enum Source<'a> {
    /// `Variable(slot)`: a parameter slot or an object rest's copy.
    Variable(&'a str),
    /// `input[index]`: an array pattern's element read.
    Element(&'a str, usize),
    /// `input[key]`: an object pattern's property read through its key slot.
    Property(&'a str, &'a str),
    /// `slice(input, n)`: an array rest's tail.
    Slice(&'a str, usize),
    /// `temp === undefined ? default : temp`: the choice a
    /// `target = default` binding makes of the incoming value.
    Choice(&'a str),
}

impl<'p> Printer<'p> {
    /// The parameters a function spells and the body its prologue leaves.
    ///
    /// The prologue is a strict prefix of the body's statement list: a
    /// block body is one completion list holding it ahead of the authored
    /// statements, an expression body holds it ahead of the body's
    /// `return`. A body that is not a block carries no prologue; one whose
    /// generated slots have no recognizable pattern keeps its block, and
    /// the slot itself names the refusal.
    ///
    /// `accepts_rest` is the flag a `__lashlang_closure` wrap carries: the
    /// last parameter spells `...name`.
    pub(super) fn signature<'a>(
        &self,
        function: &'a FunctionExpr,
        context: &'static str,
        accepts_rest: bool,
    ) -> Result<Signature<'a>, TypeScriptSourceError> {
        if accepts_rest && function.params.is_empty() {
            return Err(TypeScriptSourceError::Unrepresentable {
                kind: "a rest parameter on a function with no parameters",
            });
        }
        let mut bound: Vec<String> = function
            .params
            .iter()
            .map(|param| param.to_string())
            .collect();
        // A block body's statements — its prologue included — are the
        // completion list's items; an expression body's prologue leads its
        // body block. Any other body carries no prologue.
        let (leading, completion): (&[Expr], bool) = match function.body.as_ref() {
            Expr::Block(items) => match items.as_slice() {
                [
                    Expr::Role {
                        role: StructuralRole::Completion,
                        expr,
                    },
                ] => match expr.as_ref() {
                    Expr::Block(inner) => (inner.as_slice(), true),
                    _ => (items.as_slice(), false),
                },
                _ => (items.as_slice(), false),
            },
            Expr::Role {
                role: StructuralRole::Completion,
                expr,
            } => match expr.as_ref() {
                Expr::Block(inner) => (inner.as_slice(), true),
                _ => (&[], false),
            },
            _ => {
                return Ok(Signature {
                    params: self.plain_params(function, context)?,
                    body: Cow::Borrowed(function.body.as_ref()),
                    bound,
                    arguments: None,
                    required: function.params.len(),
                });
            }
        };
        let mut rest = leading;
        // A function that reads `arguments` snapshots it ahead of the
        // parameters the object belongs to; the slot is generated and the
        // body's reads of it display as `arguments`.
        let arguments = rest
            .split_first()
            .and_then(|(first, _)| arguments_prologue(first))
            .map(str::to_string);
        if let Some(slot) = &arguments {
            bound.push(slot.clone());
            rest = &rest[1..];
        }
        let mut params = Vec::with_capacity(function.params.len());
        let mut required = 0usize;
        let mut counting = true;
        for (index, param) in function.params.iter().enumerate() {
            let is_rest = accepts_rest && index + 1 == function.params.len();
            let (mut spelling, defaulted) = if param.as_str().starts_with(LOWERED_BINDING_PREFIX) {
                match self.parameter_pattern(param.as_str(), rest, &mut bound)? {
                    Some(found) => {
                        rest = &rest[found.used..];
                        (found.spelling, found.defaulted)
                    }
                    // The slot's prologue is no pattern; its generated name
                    // names the refusal.
                    None => (self.binding_identifier(context, param.as_str())?, false),
                }
            } else {
                (self.binding_identifier(context, param.as_str())?, false)
            };
            if is_rest {
                if !is_typescript_identifier(spelling.as_str()) {
                    return Err(TypeScriptSourceError::Unrepresentable {
                        kind: "a rest parameter that is not a name",
                    });
                }
                spelling = format!("...{spelling}");
            }
            // A rest parameter, and every parameter past the first default,
            // is past the arity a call must supply.
            if counting && (defaulted || is_rest) {
                counting = false;
            }
            if counting {
                required += 1;
            }
            params.push(spelling);
        }
        let consumed = leading.len() - rest.len();
        let body = if consumed == 0 {
            Cow::Borrowed(function.body.as_ref())
        } else if completion {
            // What the prologue leaves is still the body block's
            // completion list.
            Cow::Owned(Expr::Role {
                role: StructuralRole::Completion,
                expr: Box::new(Expr::Block(rest.to_vec())),
            })
        } else {
            // An expression body's prologue leaves exactly the `return`.
            let [tail] = rest else {
                return Err(TypeScriptSourceError::Unrepresentable {
                    kind: "a parameter prologue that does not cover the body prefix",
                });
            };
            Cow::Borrowed(tail)
        };
        Ok(Signature {
            params,
            body,
            bound,
            arguments,
            required,
        })
    }

    fn plain_params(
        &self,
        function: &FunctionExpr,
        context: &'static str,
    ) -> Result<Vec<String>, TypeScriptSourceError> {
        function
            .params
            .iter()
            .map(|param| self.binding_identifier(context, param.as_str()))
            .collect()
    }

    /// The pattern a generated parameter slot's prologue segment spells, or
    /// `None` when the segment is not one the lowerer emits.
    fn parameter_pattern<'a>(
        &self,
        slot: &'a str,
        items: &'a [Expr],
        bound: &mut Vec<String>,
    ) -> Result<Option<PatternMatch<'a>>, TypeScriptSourceError> {
        self.pattern_in(items, &Source::Variable(slot), bound)
    }

    /// The pattern `lower_pattern(pattern, source)` emitted at the head of
    /// `items`, or `None` when they open with no pattern the lowerer writes.
    fn pattern_in<'a>(
        &self,
        items: &'a [Expr],
        source: &Source<'a>,
        bound: &mut Vec<String>,
    ) -> Result<Option<PatternMatch<'a>>, TypeScriptSourceError> {
        let Some(Expr::Assign { target, expr }) = items.first() else {
            return Ok(None);
        };
        if !target.is_simple() {
            return Ok(None);
        }
        let generated = target.root.as_str().starts_with(LOWERED_BINDING_PREFIX);
        // `[..]`: the pattern copies the incoming iterable into a generated
        // slot the element reads index.
        if generated
            && let Some([input]) = stdlib_call(expr.as_ref(), "Lash.ArrayFromIterable")
            && let Some(carried) = match_source(source, input)
        {
            return self.array_pattern(target.root.as_str(), items, bound, carried);
        }
        let Some(carried) = match_source(source, expr.as_ref()) else {
            return Ok(None);
        };
        if !generated {
            // `name` — bound in place, boxed when the binding is a cell.
            bound.push(target.root.to_string());
            return Ok(Some(PatternMatch {
                spelling: self.binding_identifier("function parameter", target.root.as_str())?,
                used: 1,
                carried,
                defaulted: false,
            }));
        }
        // `{..}`: a generated slot holds the incoming value, a
        // null-or-undefined check throws on it, and one key read follows per
        // property.
        if let Some(check @ Expr::If { .. }) = items.get(1)
            && require_object_coercible(check, target.root.as_str())
        {
            return self.object_pattern(target.root.as_str(), items, bound, carried);
        }
        // `target = default`: the slot holds the incoming value and the
        // target binds the `slot === undefined` choice of it.
        if let Some(inner) =
            self.pattern_in(&items[1..], &Source::Choice(target.root.as_str()), bound)?
        {
            // A `Choice` source always hands its default up.
            let Some(default) = inner.carried else {
                return Ok(None);
            };
            return Ok(Some(PatternMatch {
                spelling: format!("{} = {}", inner.spelling, self.expression(default)?),
                used: inner.used + 1,
                carried,
                defaulted: true,
            }));
        }
        Ok(None)
    }

    /// The `[e0, e1, .., ...rest]` an array pattern's prologue spells: a
    /// generated iterable copy `input`, then one element pattern per index
    /// and a `slice` for the rest.
    fn array_pattern<'a>(
        &self,
        input: &'a str,
        items: &'a [Expr],
        bound: &mut Vec<String>,
        carried: Option<&'a Expr>,
    ) -> Result<Option<PatternMatch<'a>>, TypeScriptSourceError> {
        let mut elements: Vec<String> = Vec::new();
        let mut position = 0usize;
        let mut used = 1usize;
        // Each element is a pattern bound to `input[index]`; a hole emits
        // nothing, so the read's index — not the element count — gives the
        // position.
        while let Some(index) = items.get(used).and_then(|item| element_index(item, input)) {
            while position < index {
                elements.push(String::new());
                position += 1;
            }
            let Some(element) =
                self.pattern_in(&items[used..], &Source::Element(input, index), bound)?
            else {
                return Ok(None);
            };
            elements.push(element.spelling);
            position += 1;
            used += element.used;
        }
        // `...rest`: the positions past the pattern's length, sliced off.
        if let Some(element) =
            self.pattern_in(&items[used..], &Source::Slice(input, position), bound)?
        {
            elements.push(format!("...{}", element.spelling));
            used += element.used;
        }
        if elements.is_empty() {
            return Ok(None);
        }
        Ok(Some(PatternMatch {
            spelling: format!("[{}]", elements.join(", ")),
            used,
            carried,
            defaulted: false,
        }))
    }

    /// The `{k, k: b, ..}` an object pattern's prologue spells: a generated
    /// copy of the incoming object, its null check, then one key slot and
    /// property pattern per property.
    fn object_pattern<'a>(
        &self,
        input: &'a str,
        items: &'a [Expr],
        bound: &mut Vec<String>,
        carried: Option<&'a Expr>,
    ) -> Result<Option<PatternMatch<'a>>, TypeScriptSourceError> {
        let mut used = 2usize;
        let mut properties: Vec<String> = Vec::new();
        let mut keys: Vec<String> = Vec::new();
        while let Some(Expr::Assign {
            target: key_target,
            expr: key_value,
        }) = items.get(used)
        {
            if !key_target.is_simple()
                || !key_target.root.as_str().starts_with(LOWERED_BINDING_PREFIX)
            {
                break;
            }
            let Some(property) = self.pattern_in(
                &items[used + 1..],
                &Source::Property(input, key_target.root.as_str()),
                bound,
            )?
            else {
                // No property follows — the assign is the rest copy or the
                // prologue is over; what remains is a body's first
                // statement, which the caller keeps.
                break;
            };
            keys.push(key_target.root.to_string());
            let key = match key_value.as_ref() {
                Expr::String(name) if is_typescript_identifier(name.as_str()) => {
                    name.as_str().to_string()
                }
                Expr::String(name) => key(name.as_str()),
                computed => format!("[{}]", self.expression(computed)?),
            };
            // `{x}` for the property bound to its own name, `{x = d}` for
            // the default of it; anything else needs the key spelled.
            properties.push(
                if property.spelling == key || property.spelling.starts_with(&format!("{key} = ")) {
                    property.spelling
                } else {
                    format!("{key}: {}", property.spelling)
                },
            );
            used += 1 + property.used;
        }
        if properties.is_empty() {
            return Ok(None);
        }
        // `{ .., ..rest }`: the input's own enumerable keys are copied, the
        // bound ones deleted from the copy, and the rest bound to it.
        if let Some(Expr::Assign { target, expr }) = items.get(used)
            && target.is_simple()
            && target.root.as_str().starts_with(LOWERED_BINDING_PREFIX)
            && let Some([entries]) = stdlib_call(expr.as_ref(), "Object.fromEntries")
            && let Some([Expr::Variable(from)]) = stdlib_call(entries, "Object.entries")
            && from.as_str() == input
        {
            used += 1;
            for key in &keys {
                let Some(Expr::BuiltinCall { name, args }) = items.get(used) else {
                    return Ok(None);
                };
                let [Expr::Variable(copy), Expr::Variable(deleted)] = args.as_slice() else {
                    return Ok(None);
                };
                if name.as_str() != "__lashlang_heap_delete_member"
                    || copy.as_str() != target.root.as_str()
                    || deleted.as_str() != key.as_str()
                {
                    return Ok(None);
                }
                used += 1;
            }
            let Some(rest) = self.pattern_in(
                &items[used..],
                &Source::Variable(target.root.as_str()),
                bound,
            )?
            else {
                return Ok(None);
            };
            if !is_typescript_identifier(rest.spelling.as_str()) {
                return Ok(None);
            }
            properties.push(format!("...{}", rest.spelling));
            used += rest.used;
        }
        Ok(Some(PatternMatch {
            spelling: format!("{{ {} }}", properties.join(", ")),
            used,
            carried,
            defaulted: false,
        }))
    }
    /// A closure prints as an arrow, and as an `async` arrow when its own body
    /// awaits: only an async arrow lowers to a closure that awaits, and an
    /// `await` in a sync arrow does not parse.
    pub(super) fn arrow(&self, function: &FunctionExpr) -> Printed {
        self.function_literal(function, None)
    }

    /// A function's spelling. `closure` is the `(required arity, accepts
    /// rest)` a `__lashlang_closure` wrap records when the signature has
    /// defaults or a rest parameter; the wrap drops away once the signature
    /// spells them, and an arity the spelled signature does not reproduce
    /// is IR no authored program lowers to.
    pub(super) fn function_literal(
        &self,
        function: &FunctionExpr,
        closure: Option<(usize, bool)>,
    ) -> Printed {
        let accepts_rest = closure.is_some_and(|(_, rest)| rest);
        if let Some(name) = &function.name {
            return self.named_function(name.as_str(), function, closure);
        }
        // A function that reads its receiver is a `function` form: an arrow's
        // `this` is its enclosing function's.
        if let Some(receiver) = &function.receiver {
            self.receivers.borrow_mut().insert(receiver.to_string());
            let signature = self.signature(function, "function parameter", accepts_rest)?;
            self.check_closure_arity(closure, &signature)?;
            let mut bound = signature.bound.clone();
            return self.displaying_arguments(&signature, |printer| {
                Ok(format!(
                    "{}function ({}) {}",
                    if awaits_in_own_body(&function.body) {
                        "async "
                    } else {
                        ""
                    },
                    signature.params.join(", "),
                    printer.rooted_block(&signature.body, 0, &mut bound)?
                ))
            });
        }
        let signature = self.signature(function, "arrow parameter", accepts_rest)?;
        self.check_closure_arity(closure, &signature)?;
        let mut bound = signature.bound.clone();
        // A function that snapshots `arguments` is a `function` form even
        // when it never reads `this`: an arrow's `arguments` is its
        // enclosing function's, so the arrow spelling would not re-lower to
        // this function.
        if signature.arguments.is_some() {
            return self.displaying_arguments(&signature, |printer| {
                Ok(format!(
                    "{}function ({}) {}",
                    if awaits_in_own_body(&function.body) {
                        "async "
                    } else {
                        ""
                    },
                    signature.params.join(", "),
                    printer.rooted_block(&signature.body, 0, &mut bound)?
                ))
            });
        }
        // An expression-bodied arrow lowers to a body that is one `return`;
        // it prints back as an expression body, which lowers to that `return`
        // again, and a braced body would lower to a different program. The
        // `return` is the body's last block element once a parameter
        // prologue is consumed, and the block's only element otherwise.
        let body = match signature.body.as_ref() {
            Expr::Block(items) if let [Expr::FunctionReturn(value)] = items.as_slice() => {
                format!("({})", self.expression(value)?)
            }
            Expr::FunctionReturn(value) => format!("({})", self.expression(value)?),
            body => self.rooted_block(body, 0, &mut bound)?,
        };
        Ok(format!(
            "{}({}) => {body}",
            if awaits_in_own_body(&function.body) {
                "async "
            } else {
                ""
            },
            signature.params.join(", "),
        ))
    }

    /// Prints with `arguments` reads displaying as `arguments`: the slot a
    /// function's argv snapshot binds is generated, and the body's mentions
    /// of it are the authored spelling's. The rename scope is popped whether
    /// or not the print fails.
    fn displaying_arguments(
        &self,
        signature: &Signature,
        print: impl FnOnce(&Self) -> Printed,
    ) -> Printed {
        let Some(arguments) = &signature.arguments else {
            return print(self);
        };
        self.binding_names.borrow_mut().push(BTreeMap::from([(
            arguments.clone(),
            "arguments".to_string(),
        )]));
        let printed = print(self);
        self.binding_names.borrow_mut().pop();
        printed
    }

    /// A `__lashlang_closure` wrap's recorded arity against the spelled
    /// signature's: the signature's leading required parameters must be
    /// exactly the wrap's count, or the wrap belongs to IR no source
    /// re-lowers to.
    fn check_closure_arity(
        &self,
        closure: Option<(usize, bool)>,
        signature: &Signature,
    ) -> Result<(), TypeScriptSourceError> {
        if let Some((required, _)) = closure
            && signature.required != required
        {
            return Err(TypeScriptSourceError::Unrepresentable {
                kind: "a closure arity that is not its parameters' count",
            });
        }
        Ok(())
    }

    /// A function with a name of its own is a `function` form: the name is
    /// bound inside it, so an arrow (which has none) would lower to a
    /// different function.
    pub(super) fn named_function(
        &self,
        name: &str,
        function: &FunctionExpr,
        closure: Option<(usize, bool)>,
    ) -> Printed {
        if let Some(receiver) = &function.receiver {
            self.receivers.borrow_mut().insert(receiver.to_string());
        }
        let signature = self.signature(
            function,
            "function parameter",
            closure.is_some_and(|(_, rest)| rest),
        )?;
        self.check_closure_arity(closure, &signature)?;
        let mut bound = signature.bound.clone();
        self.displaying_arguments(&signature, |printer| {
            Ok(format!(
                "{}function {}({}) {}",
                if awaits_in_own_body(&function.body) {
                    "async "
                } else {
                    ""
                },
                printer.binding_identifier("function", name)?,
                signature.params.join(", "),
                printer.rooted_block(&signature.body, 0, &mut bound)?
            ))
        })
    }
}

/// `Some(None)` when `expression` is the source's shape; for a `Choice`,
/// `Some(Some(default))` where `default` is the value the `=== undefined`
/// arm lowers to — the `= default` the pattern spells.
fn match_source<'a>(source: &Source<'a>, expression: &'a Expr) -> Option<Option<&'a Expr>> {
    match source {
        Source::Variable(name) => {
            matches!(expression, Expr::Variable(found) if found.as_str() == *name).then_some(None)
        }
        Source::Element(input, index) => matches!(expression, Expr::Index { target, index: at }
                if matches!(target.as_ref(), Expr::Variable(from) if from.as_str() == *input)
                    && integer_index(at) == Some(*index))
        .then_some(None),
        Source::Property(input, key) => matches!(expression, Expr::Index { target, index }
                if matches!(target.as_ref(), Expr::Variable(from) if from.as_str() == *input)
                    && matches!(index.as_ref(), Expr::Variable(slot) if slot.as_str() == *key))
        .then_some(None),
        Source::Slice(input, start) => {
            let Some([Expr::Variable(from), number]) = stdlib_call(expression, "slice") else {
                return None;
            };
            (from.as_str() == *input && integer_index(number) == Some(*start)).then_some(None)
        }
        Source::Choice(temp) => default_choice(expression, temp).map(Some),
    }
}

/// The index `item`'s incoming value reads from `input`, when it is the
/// first statement of an array pattern's element — `input[index]` itself,
/// or the same read under the nested pattern's or default's slot.
fn element_index(item: &Expr, input: &str) -> Option<usize> {
    let Expr::Assign { target, expr } = item else {
        return None;
    };
    if !target.is_simple() {
        return None;
    }
    // A bound name's read may sit under its `cell_new` box; a nested array
    // pattern's, under its `ArrayFromIterable` copy.
    let mut value = cell_wrapped(expr);
    if let Some([inner]) = stdlib_call(value, "Lash.ArrayFromIterable") {
        value = inner;
    }
    let Expr::Index {
        target: read,
        index,
    } = value
    else {
        return None;
    };
    if !matches!(read.as_ref(), Expr::Variable(from) if from.as_str() == input) {
        return None;
    }
    integer_index(index)
}

/// The slot a function's `arguments` snapshot binds, when `statement` is
/// the `slot = __lashlang_stdlib("Lash.Arguments")` prologue it opens
/// with. The slot's name is generated; the body's reads of it are the
/// `arguments` mentions the prologue serves.
fn arguments_prologue(statement: &Expr) -> Option<&str> {
    let Expr::Assign { target, expr } = statement else {
        return None;
    };
    if !target.is_simple()
        || !stdlib_call(expr, "Lash.Arguments").is_some_and(|args| args.is_empty())
    {
        return None;
    }
    Some(target.root.as_str())
}

/// The `cell_new(value)` a cell-bound pattern element wraps its read in, or
/// `value` itself.
fn cell_wrapped(expr: &Expr) -> &Expr {
    match expr {
        Expr::BuiltinCall { name, args }
            if name.as_str() == "__lashlang_cell_new" && args.len() == 1 =>
        {
            &args[0]
        }
        other => other,
    }
}

/// The whole-number index an element read names.
fn integer_index(index: &Expr) -> Option<usize> {
    let Expr::Number(value) = index else {
        return None;
    };
    (value.fract() == 0.0 && *value >= 0.0).then_some(*value as usize)
}

/// The default a `slot === undefined` choice binds, or `None` when the
/// choice is not the pattern-default shape.
fn default_choice<'a>(expr: &'a Expr, slot: &str) -> Option<&'a Expr> {
    let Expr::If {
        condition,
        then_block,
        else_block,
    } = expr
    else {
        return None;
    };
    let Expr::CoercingBinary {
        left,
        op: CoercingBinaryOp::StrictEqual,
        right,
    } = condition.as_ref()
    else {
        return None;
    };
    if !matches!(left.as_ref(), Expr::Variable(name) if name.as_str() == slot)
        || !matches!(right.as_ref(), Expr::Absent)
        || !matches!(else_block.as_ref(), Expr::Variable(name) if name.as_str() == slot)
    {
        return None;
    }
    Some(then_block)
}

/// The `input === null || input === undefined` throw an object pattern's
/// RequireObjectCoercible leaves ahead of the property reads.
fn require_object_coercible(expr: &Expr, input: &str) -> bool {
    let Expr::If {
        condition,
        then_block,
        else_block,
    } = expr
    else {
        return false;
    };
    let is = |side: &Expr, undefined: bool| {
        matches!(side, Expr::CoercingBinary { left, op: CoercingBinaryOp::StrictEqual, right }
        if matches!(left.as_ref(), Expr::Variable(name) if name.as_str() == input)
            && if undefined {
                matches!(right.as_ref(), Expr::Absent)
            } else {
                matches!(right.as_ref(), Expr::Null)
            })
    };
    let Expr::OperandLogical {
        left,
        op: OperandLogicalOp::Or,
        right,
    } = condition.as_ref()
    else {
        return false;
    };
    is(left.as_ref(), false)
        && is(right.as_ref(), true)
        && matches!(then_block.as_ref(), Expr::Throw(_))
        && matches!(else_block.as_ref(), Expr::Absent)
}
/// Whether `expr` awaits in its own function body: a nested closure or process
/// body awaits on its own account.
fn awaits_in_own_body(expr: &Expr) -> bool {
    match expr {
        Expr::Await(_) => true,
        Expr::Function(_) | Expr::ProcessLiteral(_) => false,
        _ => expr.children().any(awaits_in_own_body),
    }
}
