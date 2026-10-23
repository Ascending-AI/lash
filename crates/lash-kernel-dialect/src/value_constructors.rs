//! Explicit value constructors shared by ordinary source dialects.
use lash_kernel_doc::{self as k, Expr, Literal};

#[derive(Clone, Copy)]
pub enum Constructor {
    Error,
    Timestamp,
    Integer,
    Bytes,
    Tuple,
    Map,
    Set,
    Record,
    Absent,
    Index,
    CallToken,
    Native(&'static str, usize),
}
impl Constructor {
    const ALL: [(Self, &'static str); 25] = [
        (Self::Error, "error.new"),
        (Self::Timestamp, "timestamp.from_nanoseconds"),
        (Self::Integer, "integer.from_decimal"),
        (Self::Bytes, "bytes.from_hex"),
        (Self::Tuple, "tuple.of"),
        (Self::Map, "map.from_entries"),
        (Self::Set, "set.of"),
        (Self::Record, "record.new"),
        (Self::Absent, "value.absent"),
        (Self::Index, "tuple.at"),
        (Self::CallToken, "callable.invoke_token"),
        (Self::Native("num.add", 2), "num.add"),
        (Self::Native("num.sub", 2), "num.sub"),
        (Self::Native("num.mul", 2), "num.mul"),
        (Self::Native("num.div", 2), "num.div"),
        (Self::Native("num.rem", 2), "num.rem"),
        (Self::Native("num.pow", 2), "num.pow"),
        (Self::Native("num.lt", 2), "num.lt"),
        (Self::Native("num.le", 2), "num.le"),
        (Self::Native("num.gt", 2), "num.gt"),
        (Self::Native("num.ge", 2), "num.ge"),
        (Self::Native("num.eq", 2), "num.eq"),
        (Self::Native("num.neg", 1), "num.neg"),
        (Self::Native("num.to_float", 1), "num.to_float"),
        (Self::Native("list.len", 1), "list.len"),
    ];
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.iter().find(|(_, n)| *n == name).map(|(c, _)| *c)
    }
    pub fn reserves(root: &str) -> bool {
        Self::ALL
            .iter()
            .any(|(_, name)| name.split_once('.').is_some_and(|(r, _)| r == root))
    }
    /// Constructor arguments retain their evaluation order. Scalar text
    /// constructors deliberately accept literals, giving exact kernel kinds.
    pub fn build(
        self,
        mut args: Vec<Expr>,
        helper: impl FnOnce(&str) -> Option<lash_vocabulary::FunctionId>,
    ) -> Result<Expr, String> {
        match self {
            Self::Native(name, arity) => {
                if args.len() != arity {
                    return Err(format!("{name} requires {arity} arguments"));
                }
                Ok(Expr::Call {
                    function: helper(name).ok_or_else(|| format!("missing helper {name}"))?,
                    args,
                })
            }
            Self::Error | Self::Timestamp => {
                let (name, arity) = if matches!(self, Self::Error) {
                    ("error.new", 3)
                } else {
                    ("timestamp.from_nanoseconds", 1)
                };
                if args.len() != arity {
                    return Err(format!("{name} requires {arity} arguments"));
                }
                Ok(Expr::Call {
                    function: helper(name).ok_or_else(|| format!("missing helper {name}"))?,
                    args,
                })
            }
            Self::Integer | Self::Bytes => {
                let [Expr::Literal(Literal::Text(text))] = args.as_slice() else {
                    return Err("the exact scalar constructor requires one text literal".into());
                };
                Ok(Expr::Literal(if matches!(self, Self::Integer) {
                    Literal::Int(k::Integer::parse(text).map_err(|e| e.to_string())?)
                } else {
                    Literal::Bytes(k::Bytes::parse_hex(text).map_err(|e| e.to_string())?)
                }))
            }
            Self::Index if args.len() == 2 => {
                let [target, index]: [Expr; 2] = args
                    .try_into()
                    .map_err(|_| "tuple.at requires two arguments")?;
                Ok(Expr::Member(Box::new(k::Member::Index { target, index })))
            }
            Self::Tuple => Ok(Expr::Tuple(args)),
            Self::Set => Ok(Expr::Set(args)),
            Self::Absent if args.is_empty() => Ok(Expr::Literal(Literal::Absent)),
            Self::Map => {
                let mut entries = vec![];
                for pair in args {
                    let Expr::List(pair) = pair else {
                        return Err("map entries require two-element lists".into());
                    };
                    let [key, value]: [Expr; 2] = pair
                        .try_into()
                        .map_err(|_| "map entries require two elements")?;
                    entries.push(k::MapEntry { key, value });
                }
                Ok(Expr::Map(entries))
            }
            Self::Record if args.len() == 1 => match args.remove(0) {
                Expr::Record(entries) => Ok(Expr::Record(entries)),
                Expr::Map(entries) => Ok(Expr::Record(
                    entries
                        .into_iter()
                        .map(|entry| {
                            let Expr::Literal(Literal::Text(field)) = entry.key else {
                                return Err("record keys must be text literals".to_owned());
                            };
                            Ok(k::RecordEntry {
                                field,
                                value: entry.value,
                            })
                        })
                        .collect::<Result<_, String>>()?,
                )),
                _ => Err("record.new requires a record or a literal text-keyed map".into()),
            },
            _ => Err("invalid value constructor arguments".into()),
        }
    }
}
