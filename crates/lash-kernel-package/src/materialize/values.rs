//! Exact value kinds, with paths and expansion bounds checked before construction.
use super::*;

pub(super) struct Values<'a> {
    nodes: &'a [&'a FunctionValue],
    numbers: NumberPolicy,
    helpers: &'a BuildHelpers,
    pub names: Vec<Name>,
    pub library: BTreeSet<FunctionId>,
    count: u64,
}
impl<'a> Values<'a> {
    pub fn new(
        nodes: &'a [&'a FunctionValue],
        numbers: NumberPolicy,
        helpers: &'a BuildHelpers,
    ) -> Self {
        Self {
            nodes,
            numbers,
            helpers,
            names: vec![],
            library: BTreeSet::new(),
            count: 0,
        }
    }
    fn bound(&mut self, depth: usize, bytes: usize) -> Result<(), MaterializeRefusal> {
        if depth > 48 {
            return Err(MaterializeRefusal::LimitExceeded {
                resource: "value depth".into(),
                limit: 48,
                actual: depth as u64,
            });
        }
        self.count = self.count.saturating_add(bytes as u64 + 1);
        if self.count > FunctionValueLimits::CEILING {
            return Err(MaterializeRefusal::LimitExceeded {
                resource: "expanded values".into(),
                limit: FunctionValueLimits::CEILING,
                actual: self.count,
            });
        }
        Ok(())
    }
    fn helper(&mut self, name: &str, args: Vec<Expr>) -> Result<Expr, MaterializeRefusal> {
        let id = self
            .helpers
            .functions()
            .named(name)
            .find(|id| self.helpers.contains(id))
            .ok_or_else(|| MaterializeRefusal::MissingHelper { name: name.into() })?;
        self.library.insert(id);
        Ok(Expr::Call { function: id, args })
    }
    pub fn capture(
        &mut self,
        node: u32,
        path: &str,
        value: &CaptureValue,
        depth: usize,
    ) -> Result<Expr, MaterializeRefusal> {
        self.bound(depth, 0)?;
        Ok(match value {
            CaptureValue::Data(datum) => return self.datum(node, path, datum, depth),
            CaptureValue::Function(function) => {
                let index = self
                    .nodes
                    .iter()
                    .position(|n| std::ptr::eq(*n, function.as_ref()))
                    .ok_or(MaterializeRefusal::InvalidReference {
                        node,
                        index: u32::MAX,
                    })?;
                Expr::Variable(self.names[index].clone())
            }
            CaptureValue::Reference(index) => Expr::Variable(
                self.names
                    .get(*index as usize)
                    .ok_or(MaterializeRefusal::InvalidReference {
                        node,
                        index: *index,
                    })?
                    .clone(),
            ),
            CaptureValue::Tuple(items) | CaptureValue::List(items) | CaptureValue::Set(items) => {
                let items = items
                    .iter()
                    .enumerate()
                    .map(|(i, v)| self.capture(node, &format!("{path}.{i}"), v, depth + 1))
                    .collect::<Result<_, _>>()?;
                match value {
                    CaptureValue::Tuple(_) => Expr::Tuple(items),
                    CaptureValue::List(_) => Expr::List(items),
                    _ => Expr::Set(items),
                }
            }
            CaptureValue::Record(items) => Expr::Record(
                items
                    .iter()
                    .map(|(key, v)| {
                        self.bound(depth, key.len())?;
                        Ok(RecordEntry {
                            field: key.clone(),
                            value: self.capture(node, &format!("{path}.{key}"), v, depth + 1)?,
                        })
                    })
                    .collect::<Result<_, MaterializeRefusal>>()?,
            ),
            CaptureValue::Map(items) => Expr::Map(
                items
                    .iter()
                    .enumerate()
                    .map(|(i, (k, v))| {
                        Ok(MapEntry {
                            key: self.capture(node, &format!("{path}.{i}.key"), k, depth + 1)?,
                            value: self.capture(
                                node,
                                &format!("{path}.{i}.value"),
                                v,
                                depth + 1,
                            )?,
                        })
                    })
                    .collect::<Result<_, MaterializeRefusal>>()?,
            ),
            CaptureValue::Error {
                kind,
                message,
                data,
            } => {
                self.bound(depth, kind.len().saturating_add(message.len()))?;
                let data = self.capture(node, &format!("{path}.data"), data, depth + 1)?;
                self.helper(
                    "error.new",
                    vec![
                        Expr::Literal(Literal::Text(kind.clone())),
                        Expr::Literal(Literal::Text(message.clone())),
                        data,
                    ],
                )?
            }
        })
    }
    pub fn datum(
        &mut self,
        node: u32,
        path: &str,
        value: &Datum,
        depth: usize,
    ) -> Result<Expr, MaterializeRefusal> {
        self.bound(
            depth,
            match value {
                Datum::Text(s) => s.len(),
                Datum::Bytes(b) => b.as_slice().len(),
                Datum::Int(i) => (i.bits() / 8) as usize,
                Datum::Number(token) => token.as_str().len(),
                Datum::Timestamp(t) => (t.nanoseconds.bits() / 8) as usize,
                _ => 0,
            },
        )?;
        let literal = match value {
            Datum::Null => Literal::Null,
            Datum::Absent => Literal::Absent,
            Datum::Bool(v) => Literal::Bool(*v),
            Datum::Int(v) => Literal::Int(v.clone()),
            Datum::Float(v) => Literal::Float(*v),
            Datum::Text(v) => Literal::Text(v.clone()),
            Datum::Bytes(v) => Literal::Bytes(v.clone()),
            Datum::Number(_) => {
                let decoded = Decoder {
                    max_depth: 48,
                    policy: self.numbers,
                    declared: &|_| false,
                }
                .decode(value, &lash_kernel_value::Type::Any)?;
                return self.datum(node, path, &decoded, depth);
            }
            Datum::Timestamp(v) => {
                return self.helper(
                    "timestamp.from_nanoseconds",
                    vec![Expr::Literal(Literal::Int(v.nanoseconds.clone()))],
                );
            }
            Datum::Tuple(items) | Datum::List(items) | Datum::Set(items) => {
                let items = items
                    .iter()
                    .enumerate()
                    .map(|(i, v)| self.datum(node, &format!("{path}.{i}"), v, depth + 1))
                    .collect::<Result<_, _>>()?;
                return Ok(match value {
                    Datum::Tuple(_) => Expr::Tuple(items),
                    Datum::List(_) => Expr::List(items),
                    _ => Expr::Set(items),
                });
            }
            Datum::Record(items) => {
                return Ok(Expr::Record(
                    items
                        .iter()
                        .map(|(key, v)| {
                            self.bound(depth, key.len())?;
                            Ok(RecordEntry {
                                field: key.clone(),
                                value: self.datum(node, &format!("{path}.{key}"), v, depth + 1)?,
                            })
                        })
                        .collect::<Result<_, MaterializeRefusal>>()?,
                ));
            }
            Datum::Map(items) => {
                return Ok(Expr::Map(
                    items
                        .iter()
                        .enumerate()
                        .map(|(i, (k, v))| {
                            Ok(MapEntry {
                                key: self.datum(node, &format!("{path}.{i}.key"), k, depth + 1)?,
                                value: self.datum(
                                    node,
                                    &format!("{path}.{i}.value"),
                                    v,
                                    depth + 1,
                                )?,
                            })
                        })
                        .collect::<Result<_, MaterializeRefusal>>()?,
                ));
            }
            Datum::Error(e) => {
                self.bound(depth, e.kind.len().saturating_add(e.message.len()))?;
                let data = self.datum(node, &format!("{path}.data"), &e.data, depth + 1)?;
                return self.helper(
                    "error.new",
                    vec![
                        Expr::Literal(Literal::Text(e.kind.clone())),
                        Expr::Literal(Literal::Text(e.message.clone())),
                        data,
                    ],
                );
            }
            Datum::Handle(_) | Datum::Function(_) | Datum::FunctionValue(_) => {
                return Err(MaterializeRefusal::UnrepresentableValue {
                    node,
                    path: path.into(),
                    reason: if matches!(value, Datum::Handle(_)) {
                        UnrepresentableValue::LiveHandle
                    } else {
                        UnrepresentableValue::OpaqueValue
                    },
                });
            }
        };
        Ok(Expr::Literal(literal))
    }
}
