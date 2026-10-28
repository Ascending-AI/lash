//! Indexed access alternatives and their source forms.
use crate::scope::Ty;
use lash_kernel_doc::{Expr, Literal};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Form {
    Index,
    Slice,
    TypeName,
    Store,
}
impl Form {
    fn arity(self) -> usize {
        match self {
            Self::Index => 2,
            Self::Slice => 4,
            Self::TypeName => 1,
            Self::Store => 3,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Declaration {
    pub helper: &'static str,
    pub inverse: Form,
}
pub(crate) const INDEX: Declaration = Declaration {
    helper: "py.getitem",
    inverse: Form::Index,
};
pub(crate) const SLICE: Declaration = Declaration {
    helper: "py.slice",
    inverse: Form::Slice,
};
pub(crate) const TYPE_NAME: Declaration = Declaration {
    helper: "py.type",
    inverse: Form::TypeName,
};
pub(crate) const STORE: Declaration = Declaration {
    helper: "py.setitem",
    inverse: Form::Store,
};

#[derive(Clone, Copy)]
enum Key {
    Any,
    Int,
    Text,
}
impl Key {
    fn admits(self, proven: Ty, value: &Expr) -> bool {
        match self {
            Self::Any => true,
            Self::Int => proven == Ty::Int || matches!(value, Expr::Literal(Literal::Int(_))),
            Self::Text => proven == Ty::Str || matches!(value, Expr::Literal(Literal::Text(_))),
        }
    }
}
struct Read {
    receiver: Option<Ty>,
    key: Key,
    call: Declaration,
}
const READS: &[Read] = &[
    Read {
        receiver: Some(Ty::Dict),
        key: Key::Text,
        call: Declaration {
            helper: "py.dict_getitem",
            ..INDEX
        },
    },
    Read {
        receiver: Some(Ty::List),
        key: Key::Int,
        call: Declaration {
            helper: "py.list_getitem",
            ..INDEX
        },
    },
    Read {
        receiver: Some(Ty::Tuple),
        key: Key::Int,
        call: Declaration {
            helper: "py.tuple_getitem",
            ..INDEX
        },
    },
    Read {
        receiver: None,
        key: Key::Any,
        call: INDEX,
    },
];

#[derive(Clone, Copy)]
pub(crate) enum Store {
    Member,
    Call(Declaration),
}
struct Write {
    receiver: Option<Ty>,
    key: Key,
    store: Store,
}
const WRITES: &[Write] = &[
    Write {
        receiver: Some(Ty::Dict),
        key: Key::Any,
        store: Store::Member,
    },
    Write {
        receiver: Some(Ty::List),
        key: Key::Int,
        store: Store::Call(Declaration {
            helper: "py.list_setitem",
            ..STORE
        }),
    },
    Write {
        receiver: None,
        key: Key::Any,
        store: Store::Call(STORE),
    },
];

pub(crate) fn read(receiver: Ty, proven: Ty, key: &Expr) -> Declaration {
    READS
        .iter()
        .find(|entry| {
            entry.receiver.is_none_or(|kind| kind == receiver) && entry.key.admits(proven, key)
        })
        .unwrap_or_else(|| unreachable!("the final read accepts every receiver and key"))
        .call
}
pub(crate) fn write(receiver: Ty, key: &Expr) -> Store {
    WRITES
        .iter()
        .find(|entry| {
            entry.receiver.is_none_or(|kind| kind == receiver) && entry.key.admits(Ty::Unknown, key)
        })
        .unwrap_or_else(|| unreachable!("the final write accepts every receiver and key"))
        .store
}
pub(crate) fn inverse(helper: &str, arity: usize) -> Option<Form> {
    READS
        .iter()
        .map(|entry| entry.call)
        .chain(WRITES.iter().filter_map(|entry| match entry.store {
            Store::Member => None,
            Store::Call(call) => Some(call),
        }))
        .chain([SLICE, TYPE_NAME])
        .find(|entry| entry.helper == helper && entry.inverse.arity() == arity)
        .map(|entry| entry.inverse)
}

pub(crate) const METHOD_PREFIX: &str = "py.method.";
pub(crate) fn method_name(name: &str) -> String {
    format!("{METHOD_PREFIX}{name}")
}
