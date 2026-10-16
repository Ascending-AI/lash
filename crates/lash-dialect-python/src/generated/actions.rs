//! Native nested actions share the expression grammar and its depth accounting.
use super::{Cursor, Reader, Result, invalid, k};

impl Reader<'_> {
    pub(super) fn nested_action_suite(
        &mut self,
        op: &str,
        c: &mut Cursor<'_>,
        indent: usize,
        depth: usize,
    ) -> Result<k::Expr> {
        let kind = match op {
            "call" => k::ActKind::Call(self.nested_callee(c)?),
            "spawn" => k::ActKind::Spawn(self.nested_callee(c)?),
            "perform" => {
                let effect = k::EffectName::new(c.string()?).map_err(|e| invalid(e.to_string()))?;
                c.expect(",")?;
                let result = c.metadata()?;
                c.expect(",")?;
                let then_yield = flag(c)?;
                k::ActKind::Perform {
                    effect,
                    result,
                    then_yield,
                }
            }
            "sleep" => k::ActKind::Sleep {
                then_yield: flag(c)?,
            },
            "join" => k::ActKind::Join,
            "join_many" => k::ActKind::JoinMany(match c.string()?.as_str() {
                "all" => k::JoinMode::All,
                "all_settled" => k::JoinMode::AllSettled,
                "race" => k::JoinMode::Race,
                "any" => k::JoinMode::Any,
                _ => return Err(invalid("unknown join mode")),
            }),
            "await_" => k::ActKind::Await(self.id(c)?),
            _ => return Err(invalid("unknown nested action")),
        };
        c.end(true)?;
        let mut args = vec![];
        if !self.empty(indent) {
            if !self.more(indent) {
                return Err(invalid("empty expression suites require pass"));
            }
            while self.more(indent) {
                args.push(self.expr(indent, depth)?);
            }
        }
        Ok(k::Expr::Act(Box::new(k::ActExpr { kind, args })))
    }
    pub(super) fn nested_callee(&self, c: &mut Cursor<'_>) -> Result<k::Callee> {
        Ok(if c.take("declared.") {
            k::Callee::Declared(self.name(c)?)
        } else if c.text.trim_start().starts_with("helper_") {
            k::Callee::Library(self.id(c)?)
        } else {
            k::Callee::Value(self.name(c)?)
        })
    }
    /// Consume the primary following `await`, retaining every expression child.
    pub(super) fn awaited_expr(
        &self,
        c: &mut Cursor<'_>,
        nesting: usize,
    ) -> Result<(k::ActExpr, usize)> {
        if c.take("(") {
            let (value, height) = self.inline_value(c, nesting + 1)?;
            c.expect(")")?;
            self.depth(height)?;
            let policy = self
                .operators
                .unique("py.await")
                .ok_or_else(|| invalid("`await` of a value needs the base's one `py.await`"))?;
            return Ok((
                k::ActExpr {
                    kind: k::ActKind::Await(policy),
                    args: vec![value],
                },
                height + 1,
            ));
        }
        if c.text.trim_start().starts_with("tools.") || c.text.trim_start().starts_with("tools[") {
            c.expect("tools")?;
            let name = if c.take("[") {
                let name = c.string()?;
                c.expect("]")?;
                name
            } else {
                c.expect(".")?;
                let mut name = c.word()?.to_owned();
                while c.take(".") {
                    name.push('.');
                    name.push_str(c.word()?);
                }
                name
            };
            let effect = k::EffectName::new(name).map_err(|e| invalid(e.to_string()))?;
            let mut result = None;
            let mut then_yield = false;
            let mut args = vec![];
            let mut height = 1;
            c.expect("(")?;
            if !c.take(")") {
                loop {
                    if c.take("then_yield=True") {
                        then_yield = true;
                        c.expect(")")?;
                        break;
                    }
                    if result.is_none() && c.take("result=") {
                        result = Some(c.metadata()?);
                        if c.take(")") {
                            break;
                        }
                        c.expect(",")?;
                        continue;
                    }
                    if result.is_some() {
                        return Err(invalid("a tool call's arguments come before `result=`"));
                    }
                    let (arg, h) = self.inline_value(c, nesting + 1)?;
                    self.depth(h)?;
                    height = height.max(h + 1);
                    args.push(arg);
                    if c.take(")") {
                        break;
                    }
                    c.expect(",")?;
                }
            }
            return Ok((
                k::ActExpr {
                    kind: k::ActKind::Perform {
                        effect,
                        result: result.unwrap_or(k::Type::Any),
                        then_yield,
                    },
                    args,
                },
                height,
            ));
        }
        let mut probe = Cursor::new(c.text);
        let primitive = probe.word().ok().and_then(|name| match name {
            "sleep" => Some(k::ActKind::Sleep { then_yield: false }),
            "join" => Some(k::ActKind::Join),
            "join_all" => Some(k::ActKind::JoinMany(k::JoinMode::All)),
            "join_all_settled" => Some(k::ActKind::JoinMany(k::JoinMode::AllSettled)),
            "join_race" => Some(k::ActKind::JoinMany(k::JoinMode::Race)),
            "join_any" => Some(k::ActKind::JoinMany(k::JoinMode::Any)),
            _ => None,
        });
        if let Some(mut kind) = primitive {
            c.text = probe.text;
            c.expect("(")?;
            let (arg, h) = self.inline_value(c, nesting + 1)?;
            self.depth(h)?;
            if c.take(",") {
                let k::ActKind::Sleep { then_yield } = &mut kind else {
                    return Err(invalid("wrong native action arity"));
                };
                c.expect("then_yield=True")?;
                *then_yield = true;
            }
            c.expect(")")?;
            return Ok((
                k::ActExpr {
                    kind,
                    args: vec![arg],
                },
                h + 1,
            ));
        }
        let callee = self.nested_callee(c)?;
        if !c.text.trim_start().starts_with('(') {
            let k::Callee::Value(name) = callee else {
                return Err(invalid("a call needs arguments"));
            };
            let policy = self
                .operators
                .unique("py.await")
                .ok_or_else(|| invalid("`await` of a value needs the base's one `py.await`"))?;
            return Ok((
                k::ActExpr {
                    kind: k::ActKind::Await(policy),
                    args: vec![k::Expr::Variable(name)],
                },
                2,
            ));
        }
        c.expect("(")?;
        let (args, height) = self.inline_items(c, ")", nesting)?;
        Ok((
            k::ActExpr {
                kind: k::ActKind::Call(callee),
                args,
            },
            height,
        ))
    }
}

fn flag(c: &mut Cursor<'_>) -> Result<bool> {
    if c.take("True") {
        Ok(true)
    } else if c.take("False") {
        Ok(false)
    } else {
        Err(invalid("expected a boolean flag"))
    }
}
