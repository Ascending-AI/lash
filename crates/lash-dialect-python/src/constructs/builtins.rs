//! Builtin calls and forwarding values share their parameter and spelling declaration.
#[derive(Clone, Copy)]
pub(crate) enum BuiltinInverse {
    Direct,
    Alternative(&'static str),
    Refused(BuiltinRefusal),
}
#[derive(Clone, Copy)]
pub(crate) enum BuiltinRefusal {
    SpecialArgumentConvention,
}

pub(crate) struct Builtin {
    pub(crate) name: &'static str,
    pub(crate) helper: &'static str,
    pub(crate) class: bool,
    pub(crate) inverse: BuiltinInverse,
    /// Parameter spelling: `/` positional only, `*` keyword only,
    /// `...` variadic positional, `**` variadic keyword; `=` omittable.
    pub(crate) params: &'static [&'static str],
}
macro_rules! builtin { ($name:literal, $helper:literal, [$($param:literal),*], $inverse:expr) => { Builtin { name: $name, helper: $helper, class: false, inverse: $inverse, params: &[$($param),*] } }; }
macro_rules! builtin_type { ($name:literal, $helper:literal, [$($param:literal),*], $inverse:expr) => { Builtin { class: true, ..builtin!($name, $helper, [$($param),*], $inverse) } }; }
pub(crate) const BUILTINS: &[Builtin] = &[
    builtin!(
        "print",
        "py.print_text",
        ["...values", "*sep=", "*end=", "*flush="],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!("len", "py.length", ["/obj"], BuiltinInverse::Direct),
    builtin_type!(
        "str",
        "py.str",
        ["/object="],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!("repr", "py.repr", ["/obj"], BuiltinInverse::Direct),
    builtin_type!("int", "py.int", ["/x=", "base="], BuiltinInverse::Direct),
    builtin_type!(
        "float",
        "py.float",
        ["/x="],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin_type!("bool", "py.truth", ["/object="], BuiltinInverse::Direct),
    builtin_type!(
        "bytes",
        "py.bytes",
        ["source=", "encoding=", "errors="],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "getattr",
        "py.getattr",
        ["/object", "/name", "/default="],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "hasattr",
        "py.hasattr",
        ["/object", "/name"],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!("abs", "py.abs", ["/x"], BuiltinInverse::Direct),
    builtin!(
        "min",
        "py.extreme",
        ["...values", "*key=", "*default="],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "max",
        "py.extreme",
        ["...values", "*key=", "*default="],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "sum",
        "py.sum",
        ["/iterable", "start="],
        BuiltinInverse::Direct
    ),
    builtin!(
        "sorted",
        "py.sorted",
        ["/iterable", "*key=", "*reverse="],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin_type!("list", "py.to_list", ["/iterable="], BuiltinInverse::Direct),
    builtin_type!(
        "tuple",
        "py.to_tuple",
        ["/iterable="],
        BuiltinInverse::Direct
    ),
    builtin_type!("set", "py.to_set", ["/iterable="], BuiltinInverse::Direct),
    builtin_type!(
        "dict",
        "py.dict",
        ["/mapping=", "**kwargs"],
        BuiltinInverse::Alternative("py.to_dict")
    ),
    builtin!(
        "range",
        "py.range",
        ["/start", "/stop=", "/step="],
        BuiltinInverse::Direct
    ),
    builtin!(
        "enumerate",
        "py.enumerate",
        ["iterable", "start="],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "zip",
        "py.zip",
        ["...iterables", "*strict="],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "reversed",
        "py.reversed",
        ["/sequence"],
        BuiltinInverse::Direct
    ),
    builtin!("any", "py.any", ["/iterable"], BuiltinInverse::Direct),
    builtin!("all", "py.all", ["/iterable"], BuiltinInverse::Direct),
    builtin!(
        "round",
        "py.round",
        ["number", "ndigits="],
        BuiltinInverse::Direct
    ),
    builtin!(
        "isinstance",
        "py.instance",
        ["/obj", "/classinfo"],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!("ord", "py.ord", ["/c"], BuiltinInverse::Direct),
    builtin!("chr", "py.chr", ["/i"], BuiltinInverse::Direct),
    builtin!("divmod", "py.divmod", ["/x", "/y"], BuiltinInverse::Direct),
    builtin_type!(
        "type",
        "py.type.value",
        ["/object"],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "iter",
        "py.iter",
        ["/iterable", "/sentinel="],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "next",
        "py.next",
        ["/iterator", "/default="],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin_type!(
        "slice",
        "py.slice.value",
        ["/start", "/stop=", "/step="],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "map",
        "py.map",
        ["/function", "...iterables"],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "filter",
        "py.filter",
        ["/function", "/iterable"],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "callable",
        "py.callable",
        ["/object"],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "format",
        "py.format_spec",
        ["/value", "/format_spec="],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "pow",
        "py.pow.mod",
        ["base", "exp", "mod="],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "hex",
        "py.hex",
        ["/number"],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "oct",
        "py.oct",
        ["/number"],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "bin",
        "py.bin",
        ["/number"],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "ascii",
        "py.ascii",
        ["/object"],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "vars",
        "py.vars",
        ["/object="],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "dir",
        "py.dir",
        ["/object="],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "globals",
        "py.namespace",
        [],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
    builtin!(
        "locals",
        "py.namespace",
        [],
        BuiltinInverse::Refused(BuiltinRefusal::SpecialArgumentConvention)
    ),
];

impl Builtin {
    pub fn printed_helper(&self) -> Result<&'static str, BuiltinRefusal> {
        match self.inverse {
            BuiltinInverse::Direct => Ok(self.helper),
            BuiltinInverse::Alternative(helper) => Ok(helper),
            BuiltinInverse::Refused(reason) => Err(reason),
        }
    }
}
