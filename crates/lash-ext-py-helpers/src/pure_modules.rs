//! Explicit installed pure modules. This catalogue owns their finite surface;
//! no host import machinery or module cache participates.

pub(crate) struct Function {
    pub(crate) module: &'static str,
    pub(crate) name: &'static str,
    pub(crate) params: &'static [&'static str],
    pub(crate) required: usize,
    implementation: Implementation,
    arguments: Arguments,
}

enum Implementation {
    Math(Pole),
    Kernel(&'static str),
}

enum Arguments {
    Direct,
    PackedTail { after: usize, name: &'static str },
}

#[derive(Clone, Copy)]
enum Pole {
    None,
    Zero,
    MinusOne,
    UnitMagnitude,
    NonpositiveInteger,
    ZeroNegativePower,
}

impl Pole {
    fn source(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Zero => {
                "if eq(x, 0) { do invoke py.fail(\"ValueError\", \"math domain error\") }\n"
            }
            Self::MinusOne => {
                "if eq(x, -1) { do invoke py.fail(\"ValueError\", \"math domain error\") }\n"
            }
            Self::UnitMagnitude => {
                "if eq(abs(x), 1) { do invoke py.fail(\"ValueError\", \"math domain error\") }\n"
            }
            Self::NonpositiveInteger => {
                "if is_integer(x) { if le(x, 0) { do invoke py.fail(\"ValueError\", \"math domain error\") } }\n"
            }
            Self::ZeroNegativePower => {
                "if eq(x, 0) { if lt(y, 0) { do invoke py.fail(\"ValueError\", \"math domain error\") } }\n"
            }
        }
    }
}

struct Module {
    name: &'static str,
    constants: &'static str,
}

const MODULES: &[Module] = &[
    Module {
        name: "math",
        constants: ", pi: 3.141592653589793, e: 2.718281828459045, tau: 6.283185307179586, inf: inf, nan: nan",
    },
    Module {
        name: "json",
        constants: "",
    },
];

/// Whether this build installs the named pure Python module.
pub fn installed(name: &str) -> bool {
    MODULES.iter().any(|module| module.name == name)
}

const fn math(name: &'static str, params: &'static [&'static str], pole: Pole) -> Function {
    Function {
        module: "math",
        name,
        params,
        required: params.len(),
        implementation: Implementation::Math(pole),
        arguments: Arguments::Direct,
    }
}

const FUNCTIONS: &[Function] = &[
    math("acos", &["x"], Pole::None),
    math("acosh", &["x"], Pole::None),
    math("asin", &["x"], Pole::None),
    math("asinh", &["x"], Pole::None),
    math("atan", &["x"], Pole::None),
    math("atanh", &["x"], Pole::UnitMagnitude),
    math("cbrt", &["x"], Pole::None),
    math("cos", &["x"], Pole::None),
    math("cosh", &["x"], Pole::None),
    math("erf", &["x"], Pole::None),
    math("erfc", &["x"], Pole::None),
    math("exp", &["x"], Pole::None),
    math("exp2", &["x"], Pole::None),
    math("expm1", &["x"], Pole::None),
    math("gamma", &["x"], Pole::NonpositiveInteger),
    math("lgamma", &["x"], Pole::NonpositiveInteger),
    math("log2", &["x"], Pole::Zero),
    math("log10", &["x"], Pole::Zero),
    math("log1p", &["x"], Pole::MinusOne),
    math("sin", &["x"], Pole::None),
    math("sinh", &["x"], Pole::None),
    math("sqrt", &["x"], Pole::None),
    math("tan", &["x"], Pole::None),
    math("tanh", &["x"], Pole::None),
    math("atan2", &["y", "x"], Pole::None),
    math("hypot", &["x", "y"], Pole::None),
    math("copysign", &["x", "y"], Pole::None),
    math("nextafter", &["x", "y"], Pole::None),
    math("remainder", &["x", "y"], Pole::None),
    math("pow", &["x", "y"], Pole::ZeroNegativePower),
    Function {
        module: "math",
        name: "log",
        params: &["x", "base"],
        required: 1,
        implementation: Implementation::Kernel(
            "if eq(x, 0) { do invoke py.fail(\"ValueError\", \"math domain error\") } if bool.not(is_nan(x)) { if lt(x, 0) { do invoke py.fail(\"ValueError\", \"math domain error\") } } let answer = math.log(x)\nif bool.not(same(base, absent)) { let denominator = math.log(base) if is_nan(denominator) { do invoke py.fail(\"ValueError\", \"math domain error\") } set answer = div(answer, denominator) }\nif is_nan(answer) { if bool.not(is_nan(x)) { do invoke py.fail(\"ValueError\", \"math domain error\") } }\nif eq(x, 0) { do invoke py.fail(\"ValueError\", \"math domain error\") }\nreturn answer",
        ),
        arguments: Arguments::Direct,
    },
    Function {
        module: "math",
        name: "floor",
        params: &["x"],
        required: 1,
        implementation: Implementation::Kernel("return num.to_int(floor(x))"),
        arguments: Arguments::Direct,
    },
    Function {
        module: "math",
        name: "ceil",
        params: &["x"],
        required: 1,
        implementation: Implementation::Kernel("return num.to_int(ceil(x))"),
        arguments: Arguments::Direct,
    },
    Function {
        module: "math",
        name: "trunc",
        params: &["x"],
        required: 1,
        implementation: Implementation::Kernel("return num.to_int(trunc(x))"),
        arguments: Arguments::Direct,
    },
    Function {
        module: "math",
        name: "isfinite",
        params: &["x"],
        required: 1,
        implementation: Implementation::Kernel("return is_finite(num.to_float(x))"),
        arguments: Arguments::Direct,
    },
    Function {
        module: "math",
        name: "isnan",
        params: &["x"],
        required: 1,
        implementation: Implementation::Kernel("return is_nan(num.to_float(x))"),
        arguments: Arguments::Direct,
    },
    Function {
        module: "math",
        name: "isinf",
        params: &["x"],
        required: 1,
        implementation: Implementation::Kernel("return is_infinite(num.to_float(x))"),
        arguments: Arguments::Direct,
    },
    Function {
        module: "math",
        name: "fabs",
        params: &["x"],
        required: 1,
        implementation: Implementation::Kernel("return abs(num.to_float(x))"),
        arguments: Arguments::Direct,
    },
    Function {
        module: "math",
        name: "degrees",
        params: &["x"],
        required: 1,
        implementation: Implementation::Kernel("return mul(num.to_float(x), 57.29577951308232)"),
        arguments: Arguments::Direct,
    },
    Function {
        module: "math",
        name: "radians",
        params: &["x"],
        required: 1,
        implementation: Implementation::Kernel("return mul(num.to_float(x), 0.017453292519943295)"),
        arguments: Arguments::Direct,
    },
    Function {
        module: "math",
        name: "factorial",
        params: &["n"],
        required: 1,
        implementation: Implementation::Kernel(
            "if bool.not(same(kind(n), \"integer\")) { do invoke py.fail(\"TypeError\", \"integer required\") } if lt(n, 0) { do invoke py.fail(\"ValueError\", \"factorial() not defined for negative values\") } let answer = 1 let i = 2 while le(i, n) metered 2 { set answer = mul(answer, i) set i = add(i, 1) } return answer",
        ),
        arguments: Arguments::Direct,
    },
    Function {
        module: "math",
        name: "gcd",
        params: &["a", "b"],
        required: 2,
        implementation: Implementation::Kernel(
            "if bool.not(same(kind(a), \"integer\")) { do invoke py.fail(\"TypeError\", \"integer required\") } if bool.not(same(kind(b), \"integer\")) { do invoke py.fail(\"TypeError\", \"integer required\") } set a = abs(a) set b = abs(b) while bool.not(eq(b, 0)) metered 2 { let remainder = rem_floor(a, b) set a = b set b = remainder } return a",
        ),
        arguments: Arguments::Direct,
    },
    Function {
        module: "json",
        name: "loads",
        params: &["s"],
        required: 1,
        implementation: Implementation::Kernel(
            "return json.decode(s, \"number\", \"by_spelling\", \"map\")",
        ),
        arguments: Arguments::Direct,
    },
    Function {
        module: "json",
        name: "dumps",
        params: &[
            "obj",
            "skipkeys",
            "ensure_ascii",
            "check_circular",
            "allow_nan",
            "cls",
            "indent",
            "separators",
            "default",
            "sort_keys",
        ],
        required: 1,
        implementation: Implementation::Kernel("return invoke py.json_dump(obj, options)"),
        arguments: Arguments::PackedTail {
            after: 1,
            name: "options",
        },
    },
];

pub(crate) fn source() -> String {
    let mut source = String::from(
        "use py.bind\nuse kind\nuse same\nuse bool.not\nuse eq\nuse lt\nuse le\nuse add\nuse mul\nuse div\nuse abs\nuse rem_floor\nuse floor\nuse ceil\nuse trunc\nuse num.to_int\nuse num.to_float\nuse is_finite\nuse is_nan\nuse is_infinite\nuse is_integer\nuse py.fail\nuse py.json_dump\nuse json.decode\n",
    );
    for function in FUNCTIONS {
        if function.module == "math" {
            source.push_str(&format!("use math.{}\n", function.name));
        }
        source.push_str(&format!("use py.{}.{}\n", function.module, function.name));
    }
    for function in FUNCTIONS {
        let params: Vec<_> = function
            .params
            .iter()
            .enumerate()
            .map(|(i, name)| {
                format!(
                    "{name}{}: Any",
                    if i < function.required { "" } else { "?" }
                )
            })
            .collect();
        source.push_str(&format!(
            "\nfunction py.{}.{}({}) -> Any\nkernel 1\ncharge 32\nbody {{\n",
            function.module,
            function.name,
            match function.arguments {
                Arguments::Direct => params.join(", "),
                Arguments::PackedTail { after, name } => {
                    let members = vec!["Any"; function.params.len() - after].join(", ");
                    format!("{}, {name}: Tuple({members})", params[..after].join(", "))
                }
            }
        ));
        if let Implementation::Math(pole) = function.implementation {
            source.push_str(pole.source());
            source.push_str(&format!(
                "let answer = math.{}({})\n",
                function.name,
                function.params.join(", ")
            ));
            source.push_str("let finite = true\nlet nan_input = false\n");
            for param in function.params {
                source.push_str(&format!("if bool.not(is_finite({param})) {{ set finite = false }}\nif is_nan({param}) {{ set nan_input = true }}\n"));
            }
            source.push_str("if is_nan(answer) { if bool.not(nan_input) { do invoke py.fail(\"ValueError\", \"math domain error\") } }\nif is_infinite(answer) { if finite { do invoke py.fail(\"OverflowError\", \"math range error\") } }\nreturn answer\n");
        } else if let Implementation::Kernel(body) = function.implementation {
            source.push_str(body);
        }
        source.push_str("\n}\n");
    }
    for installed in MODULES {
        let module = installed.name;
        source.push_str(&format!("\nfunction py.module.{module}() -> Any\nkernel 1\ncharge 64\nbody {{\nreturn {{__py_module: {module:?}"));
        source.push_str(installed.constants);
        for function in FUNCTIONS
            .iter()
            .filter(|function| function.module == module)
        {
            let params = (0..function.params.len())
                .map(|index| format!("argument{index}"))
                .collect::<Vec<_>>()
                .join(", ");
            let passed = match function.arguments {
                Arguments::Direct => params.clone(),
                Arguments::PackedTail { after, .. } => format!(
                    "{}, ({})",
                    (0..after)
                        .map(|index| format!("argument{index}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    (after..function.params.len())
                        .map(|index| format!("argument{index}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            let metadata = function
                .params
                .iter()
                .enumerate()
                .map(|(index, name)| (*name, 1, index >= function.required))
                .collect::<Vec<_>>();
            let body = format!(
                "let answer = invoke py.{module}.{}({passed}) return answer",
                function.name
            );
            let value =
                crate::helper_value(&format!("{module}.{}", function.name), &metadata, &body);
            source.push_str(&format!(", {}: {value}", function.name));
        }
        source.push_str("}\n}\n");
    }
    source
}
