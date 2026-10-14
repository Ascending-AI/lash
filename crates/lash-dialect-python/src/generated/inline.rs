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
    // real tree height as operators and postfixes are wrapped, before
    // allocating deep trees. Operators bind as Python binds them: `not` 3,
    // comparisons 4 (never chained), `+`/`-` 5, `*`/`/`/`//`/`%` 6, unary
    // minus 7 and `**` 8, right of which a unary operand may stand.
    pub(super) fn inline_value(
        &self,
        c: &mut Cursor<'_>,
        nesting: usize,
    ) -> Result<(k::Expr, usize)> {
        self.binary(c, nesting, 3)
    }
    fn binary(&self, c: &mut Cursor<'_>, nesting: usize, minimum: u8) -> Result<(k::Expr, usize)> {
        if nesting > self.limit * 2 + 2 {
            return Err(depth_error());
        }
        if minimum <= 3 && c.take("not ") {
            let (arg, h) = self.binary(c, nesting + 1, 3)?;
            return self.operator("not", vec![arg], h);
        }
        let (mut left, mut height) = self.unary(c, nesting)?;
        let mut compared = false;
        loop {
            let rest = c.text.trim_start_matches(' ');
            let Some((token, own)) = [
                ("**", 8),
                ("//", 6),
                ("==", 4),
                ("<=", 4),
                (">=", 4),
                ("is ", 4),
                ("+", 5),
                ("-", 5),
                ("*", 6),
                ("/", 6),
                ("%", 6),
                ("<", 4),
                (">", 4),
            ]
            .into_iter()
            .find(|(token, _)| rest.starts_with(token)) else {
                break;
            };
            if own == 8 {
                return Err(invalid("a negated base of ** needs parentheses"));
            }
            if own < minimum {
                break;
            }
            if own == 4 && compared {
                return Err(invalid(
                    "chained comparisons have no exact inverse; group each comparison",
                ));
            }
            c.expect(token)?;
            let (right, h) = self.binary(c, nesting + 1, if own == 4 { 5 } else { own + 1 })?;
            (left, height) = self.operator(token.trim(), vec![left, right], height.max(h))?;
            compared = own == 4;
        }
        Ok((left, height))
    }
    /// Unary minus, or a negative number literal, or a power.
    fn unary(&self, c: &mut Cursor<'_>, nesting: usize) -> Result<(k::Expr, usize)> {
        if nesting > self.limit * 2 + 2 {
            return Err(depth_error());
        }
        c.text = c.text.trim_start_matches(' ');
        if let Some(rest) = c.text.strip_prefix('-') {
            if rest.starts_with(|ch: char| ch.is_ascii_digit()) {
                let literal = match self.atom(c)? {
                    k::Atom::Literal(literal) => literal,
                    k::Atom::Variable(_) => return Err(invalid("expected a number")),
                };
                let rest = c.text.trim_start_matches(' ');
                if rest.starts_with("**") || rest.starts_with('.') || rest.starts_with('[') {
                    return Err(invalid("a negative number needs parentheses here"));
                }
                return Ok((k::Expr::Literal(literal), 1));
            }
            c.expect("-")?;
            let (arg, h) = self.unary(c, nesting + 1)?;
            return self.operator("neg", vec![arg], h);
        }
        let (base, h) = self.primary(c, nesting)?;
        if c.take("**") {
            let (exponent, e) = self.unary(c, nesting + 1)?;
            return self.operator("**", vec![base, exponent], h.max(e));
        }
        Ok((base, h))
    }
    /// The operator call `token` spells, by the base document's unique
    /// identity for it; `height` is its tallest operand's.
    fn operator(&self, token: &str, args: Vec<k::Expr>, height: usize) -> Result<(k::Expr, usize)> {
        let function = self
            .operators
            .id(token, &args, &self.scope.types)
            .ok_or_else(|| invalid("operator has no unique identity"))?;
        self.depth(height)?;
        Ok((k::Expr::Call { function, args }, height + 1))
    }
    fn primary(&self, c: &mut Cursor<'_>, nesting: usize) -> Result<(k::Expr, usize)> {
        let (mut expr, mut height) = if c.take("getattr(") {
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
                    c.expect(")")?;
                    value
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
