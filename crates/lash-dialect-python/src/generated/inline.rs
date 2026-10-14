//! Bounded native expression grammar and exact atomic operands.
use super::{Cursor, Reader, Result, depth_error, invalid, k};

impl Reader<'_> {
    pub(super) fn inline(&self, c: &mut Cursor<'_>, depth: usize) -> Result<k::Expr> {
        self.depth(depth)?;
        let (expr, height) = self.inline_value(c, 0)?;
        self.depth(depth + height - 1)?;
        Ok(expr)
    }
    // Parentheses create no Node. Bound syntax recursion separately and compute
    // real tree height as postfixes are wrapped, before allocating deep trees.
    pub(super) fn inline_value(
        &self,
        c: &mut Cursor<'_>,
        nesting: usize,
    ) -> Result<(k::Expr, usize)> {
        if nesting > self.limit * 2 + 2 {
            return Err(depth_error());
        }
        let unary = if c.take("not (") {
            Some("not")
        } else if c.take("-(") {
            Some("neg")
        } else {
            None
        };
        let (mut expr, mut height) = if let Some(token) = unary {
            let (arg, h) = self.inline_value(c, nesting + 1)?;
            let function = self
                .operators
                .id(token, std::slice::from_ref(&arg), &self.scope.types)
                .ok_or_else(|| invalid("operator has no unique identity"))?;
            c.expect(")")?;
            self.depth(h)?;
            (
                k::Expr::Call {
                    function,
                    args: vec![arg],
                },
                h + 1,
            )
        } else if c.take("getattr(") {
            let (target, h) = self.inline_value(c, nesting + 1)?;
            c.expect(",")?;
            let field = c.string()?;
            c.expect(")")?;
            self.depth(h)?;
            (
                k::Expr::Member(Box::new(k::Member::Field { target, field })),
                h + 1,
            )
        } else if c.take("[") {
            let (items, height) = self.inline_items(c, "]", nesting)?;
            (k::Expr::List(items), height)
        } else if c.take("record({") {
            let mut items = vec![];
            let mut height = 1;
            if !c.take("}") {
                loop {
                    let field = c.string()?;
                    c.expect(":")?;
                    let (value, h) = self.inline_value(c, nesting + 1)?;
                    height = height.max(h + 1);
                    self.depth(height - 1)?;
                    items.push(k::RecordEntry { field, value });
                    if c.take("}") {
                        break;
                    }
                    c.expect(",")?;
                }
            }
            c.expect(")")?;
            (k::Expr::Record(items), height)
        } else if c.take("set()") {
            (k::Expr::Set(vec![]), 1)
        } else if c.take("{") {
            if c.take("}") {
                (k::Expr::Map(vec![]), 1)
            } else {
                let (first, h) = self.inline_value(c, nesting + 1)?;
                let mut height = h + 1;
                self.depth(height - 1)?;
                if c.take(":") {
                    let (value, h) = self.inline_value(c, nesting + 1)?;
                    height = height.max(h + 1);
                    self.depth(height - 1)?;
                    let mut items = vec![k::MapEntry { key: first, value }];
                    while !c.take("}") {
                        c.expect(",")?;
                        let (key, h) = self.inline_value(c, nesting + 1)?;
                        height = height.max(h + 1);
                        self.depth(height - 1)?;
                        c.expect(":")?;
                        let (value, h) = self.inline_value(c, nesting + 1)?;
                        height = height.max(h + 1);
                        self.depth(height - 1)?;
                        items.push(k::MapEntry { key, value });
                    }
                    (k::Expr::Map(items), height)
                } else {
                    let mut items = vec![first];
                    while !c.take("}") {
                        c.expect(",")?;
                        let (value, h) = self.inline_value(c, nesting + 1)?;
                        height = height.max(h + 1);
                        self.depth(height - 1)?;
                        items.push(value);
                    }
                    (k::Expr::Set(items), height)
                }
            }
        } else if c.take("(") {
            if c.take(")") {
                (k::Expr::Tuple(vec![]), 1)
            } else {
                let value = self.inline_value(c, nesting + 1)?;
                if c.take(",") {
                    let mut items = vec![value.0];
                    let mut height = value.1 + 1;
                    self.depth(height - 1)?;
                    while !c.take(")") {
                        let (value, h) = self.inline_value(c, nesting + 1)?;
                        height = height.max(h + 1);
                        self.depth(height - 1)?;
                        items.push(value);
                        if c.take(")") {
                            break;
                        }
                        c.expect(",")?;
                    }
                    (k::Expr::Tuple(items), height)
                } else {
                    let token = [
                        "**", "//", "==", "<=", ">=", "is ", "+", "-", "*", "/", "%", "<", ">",
                    ]
                    .into_iter()
                    .find(|token| c.take(token));
                    if let Some(token) = token {
                        let (right, h) = self.inline_value(c, nesting + 1)?;
                        let args = vec![value.0, right];
                        let function = self
                            .operators
                            .id(token.trim(), &args, &self.scope.types)
                            .ok_or_else(|| invalid("operator has no unique identity"))?;
                        let height = value.1.max(h) + 1;
                        self.depth(height - 1)?;
                        c.expect(")")?;
                        (k::Expr::Call { function, args }, height)
                    } else {
                        c.expect(")")?;
                        value
                    }
                }
            }
        } else if c.text.trim_start().starts_with("helper_") {
            let function = self.id(c)?;
            c.expect("(")?;
            let mut args = vec![];
            let mut height = 1;
            if !c.take(")") {
                loop {
                    let (arg, h) = self.inline_value(c, nesting + 1)?;
                    height = height.max(h + 1);
                    self.depth(height - 1)?;
                    args.push(arg);
                    if c.take(")") {
                        break;
                    }
                    c.expect(",")?;
                }
            }
            (k::Expr::Call { function, args }, height)
        } else if c.take("k.clock()") {
            (k::Expr::Clock, 1)
        } else if c.take("k.random()") {
            (k::Expr::Random, 1)
        } else {
            let atom = self.atom(c)?;
            (
                match atom {
                    k::Atom::Variable(n) => k::Expr::Variable(n),
                    k::Atom::Literal(l) => k::Expr::Literal(l),
                },
                1,
            )
        };
        loop {
            let member = if c.take(".") {
                let field = c.word()?;
                if !crate::printer::attribute_identifier(field) {
                    return Err(invalid("reserved attribute spelling"));
                }
                self.depth(height)?;
                k::Member::Field {
                    target: expr,
                    field: field.into(),
                }
            } else if c.take("[") {
                let (index, h) = self.inline_value(c, nesting + 1)?;
                c.expect("]")?;
                height = height.max(h);
                self.depth(height)?;
                k::Member::Index {
                    target: expr,
                    index,
                }
            } else {
                break;
            };
            height += 1;
            expr = k::Expr::Member(Box::new(member));
        }
        Ok((expr, height))
    }
    pub(super) fn inline_items(
        &self,
        c: &mut Cursor<'_>,
        close: &str,
        nesting: usize,
    ) -> Result<(Vec<k::Expr>, usize)> {
        let mut items = vec![];
        let mut height = 1;
        if !c.take(close) {
            loop {
                let (item, h) = self.inline_value(c, nesting + 1)?;
                height = height.max(h + 1);
                self.depth(height - 1)?;
                items.push(item);
                if c.take(close) {
                    break;
                }
                c.expect(",")?;
            }
        }
        Ok((items, height))
    }
    pub(super) fn atom(&self, c: &mut Cursor<'_>) -> Result<k::Atom> {
        if c.take("declared.") {
            return self
                .name(c)
                .map(|name| k::Atom::Literal(k::Literal::Function(name)));
        }
        if c.take("bytes.fromhex(") {
            let text = c.string()?;
            c.expect(")")?;
            return Ok(k::Atom::Literal(k::Literal::Bytes(
                k::Bytes::parse_hex(&text).map_err(|e| invalid(e.to_string()))?,
            )));
        }
        if c.take("float(") {
            let text = c.string()?;
            c.expect(")")?;
            return Ok(k::Atom::Literal(k::Literal::Float(
                k::Float::parse(&text).map_err(|e| invalid(e.to_string()))?,
            )));
        }
        if c.text.trim_start().starts_with('"') {
            return Ok(k::Atom::Literal(k::Literal::Text(c.string()?)));
        }
        c.text = c.text.trim_start_matches(' ');
        if c.text
            .starts_with(|ch: char| ch.is_ascii_digit() || ch == '-')
        {
            let end = c
                .text
                .find(|ch: char| !ch.is_ascii_digit() && !matches!(ch, '.' | 'e' | '-'))
                .unwrap_or(c.text.len());
            let token = &c.text[..end];
            c.text = &c.text[end..];
            let literal = if token.contains(['.', 'e']) {
                let n = k::Float::parse(token).map_err(|e| invalid(e.to_string()))?;
                if !n.get().is_finite() {
                    return Err(invalid("non-finite native number"));
                }
                k::Literal::Float(n)
            } else {
                k::Literal::Int(k::Integer::parse(token).map_err(|e| invalid(e.to_string()))?)
            };
            return Ok(k::Atom::Literal(literal));
        }
        let word = c.word()?;
        let lit = match word {
            "None" => k::Literal::Null,
            "True" => k::Literal::Bool(true),
            "False" => k::Literal::Bool(false),
            "k" => {
                c.expect(".")?;
                let op = c.word()?;
                if op == "absent" {
                    return Ok(k::Atom::Literal(k::Literal::Absent));
                }
                c.expect("(")?;
                let value = c.string()?;
                c.expect(")")?;
                match op {
                    "int" => k::Literal::Int(
                        k::Integer::parse(&value).map_err(|e| invalid(e.to_string()))?,
                    ),
                    "float" => k::Literal::Float(
                        k::Float::parse(&value).map_err(|e| invalid(e.to_string()))?,
                    ),
                    "bytes" => k::Literal::Bytes(
                        k::Bytes::parse_hex(&value).map_err(|e| invalid(e.to_string()))?,
                    ),
                    "function" => k::Literal::Function(k::Name::new(value)),
                    _ => return Err(invalid("unknown exact literal")),
                }
            }
            alias => return self.named(alias).map(k::Atom::Variable),
        };
        Ok(k::Atom::Literal(lit))
    }
    pub(super) fn atoms(&self, c: &mut Cursor<'_>) -> Result<Vec<k::Atom>> {
        c.expect("[")?;
        let mut atoms = vec![];
        if c.take("]") {
            return Ok(atoms);
        }
        loop {
            atoms.push(self.atom(c)?);
            if c.take("]") {
                return Ok(atoms);
            }
            c.expect(",")?;
        }
    }
}
