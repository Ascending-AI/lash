//! Explicit installed pure modules. This catalogue owns their finite surface;
//! no host import machinery or module cache participates.

pub(crate) struct Function {
    pub(crate) module: &'static str,
    pub(crate) name: &'static str,
    pub(crate) params: &'static [&'static str],
    pub(crate) required: usize,
    body: &'static str,
}

const fn math(name: &'static str, params: &'static [&'static str]) -> Function {
    Function {
        module: "math",
        name,
        params,
        required: params.len(),
        body: "",
    }
}

const FUNCTIONS: &[Function] = &[
    math("acos", &["x"]),
    math("acosh", &["x"]),
    math("asin", &["x"]),
    math("asinh", &["x"]),
    math("atan", &["x"]),
    math("atanh", &["x"]),
    math("cbrt", &["x"]),
    math("cos", &["x"]),
    math("cosh", &["x"]),
    math("erf", &["x"]),
    math("erfc", &["x"]),
    math("exp", &["x"]),
    math("exp2", &["x"]),
    math("expm1", &["x"]),
    math("gamma", &["x"]),
    math("lgamma", &["x"]),
    math("log2", &["x"]),
    math("log10", &["x"]),
    math("log1p", &["x"]),
    math("sin", &["x"]),
    math("sinh", &["x"]),
    math("sqrt", &["x"]),
    math("tan", &["x"]),
    math("tanh", &["x"]),
    math("atan2", &["y", "x"]),
    math("hypot", &["x", "y"]),
    math("copysign", &["x", "y"]),
    math("nextafter", &["x", "y"]),
    math("remainder", &["x", "y"]),
    math("pow", &["x", "y"]),
    Function {
        module: "math",
        name: "log",
        params: &["x", "base"],
        required: 1,
        body: "if eq(x, 0) { do invoke py.fail(\"ValueError\", \"math domain error\") } if bool.not(is_nan(x)) { if lt(x, 0) { do invoke py.fail(\"ValueError\", \"math domain error\") } } let answer = math.log(x)\nif bool.not(same(base, absent)) { let denominator = math.log(base) if is_nan(denominator) { do invoke py.fail(\"ValueError\", \"math domain error\") } set answer = div(answer, denominator) }\nif is_nan(answer) { if bool.not(is_nan(x)) { do invoke py.fail(\"ValueError\", \"math domain error\") } }\nif eq(x, 0) { do invoke py.fail(\"ValueError\", \"math domain error\") }\nreturn answer",
    },
    Function {
        module: "math",
        name: "floor",
        params: &["x"],
        required: 1,
        body: "return num.to_int(floor(x))",
    },
    Function {
        module: "math",
        name: "ceil",
        params: &["x"],
        required: 1,
        body: "return num.to_int(ceil(x))",
    },
    Function {
        module: "math",
        name: "trunc",
        params: &["x"],
        required: 1,
        body: "return num.to_int(trunc(x))",
    },
    Function {
        module: "math",
        name: "isfinite",
        params: &["x"],
        required: 1,
        body: "return is_finite(num.to_float(x))",
    },
    Function {
        module: "math",
        name: "isnan",
        params: &["x"],
        required: 1,
        body: "return is_nan(num.to_float(x))",
    },
    Function {
        module: "math",
        name: "isinf",
        params: &["x"],
        required: 1,
        body: "return is_infinite(num.to_float(x))",
    },
    Function {
        module: "math",
        name: "fabs",
        params: &["x"],
        required: 1,
        body: "return abs(num.to_float(x))",
    },
    Function {
        module: "math",
        name: "degrees",
        params: &["x"],
        required: 1,
        body: "return mul(num.to_float(x), 57.29577951308232)",
    },
    Function {
        module: "math",
        name: "radians",
        params: &["x"],
        required: 1,
        body: "return mul(num.to_float(x), 0.017453292519943295)",
    },
    Function {
        module: "math",
        name: "factorial",
        params: &["n"],
        required: 1,
        body: "if bool.not(same(kind(n), \"integer\")) { do invoke py.fail(\"TypeError\", \"integer required\") } if lt(n, 0) { do invoke py.fail(\"ValueError\", \"factorial() not defined for negative values\") } let answer = 1 let i = 2 while le(i, n) metered 2 { set answer = mul(answer, i) set i = add(i, 1) } return answer",
    },
    Function {
        module: "math",
        name: "gcd",
        params: &["a", "b"],
        required: 2,
        body: "if bool.not(same(kind(a), \"integer\")) { do invoke py.fail(\"TypeError\", \"integer required\") } if bool.not(same(kind(b), \"integer\")) { do invoke py.fail(\"TypeError\", \"integer required\") } set a = abs(a) set b = abs(b) while bool.not(eq(b, 0)) metered 2 { let remainder = rem_floor(a, b) set a = b set b = remainder } return a",
    },
    Function {
        module: "json",
        name: "loads",
        params: &["s"],
        required: 1,
        body: "let value = json.parse(s, \"number\", \"by_spelling\") return invoke py.reply(value)",
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
        body: "return invoke py.json_dump(obj, options)",
    },
];

pub(crate) fn source() -> String {
    let mut source = String::from(
        "use py.bind\nuse kind\nuse same\nuse bool.not\nuse eq\nuse lt\nuse le\nuse add\nuse mul\nuse div\nuse abs\nuse rem_floor\nuse floor\nuse ceil\nuse trunc\nuse num.to_int\nuse num.to_float\nuse is_finite\nuse is_nan\nuse is_infinite\nuse is_integer\nuse py.fail\nuse py.reply\nuse py.json_dump\nuse json.parse\n",
    );
    for function in FUNCTIONS {
        if function.module == "math" {
            source.push_str(&format!("use math.{}\n", function.name));
        }
        source.push_str(&format!("use py.{}.{}\n", function.module, function.name));
    }
    for function in FUNCTIONS {
        let packed = function.module == "json" && function.name == "dumps";
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
            if packed {
                "obj: Any, options: Tuple(Any, Any, Any, Any, Any, Any, Any, Any, Any)".into()
            } else {
                params.join(", ")
            }
        ));
        if function.body.is_empty() {
            let pole = match function.name {
                "log2" | "log10" => Some(
                    "if eq(x, 0) { do invoke py.fail(\"ValueError\", \"math domain error\") }\n",
                ),
                "log1p" => Some(
                    "if eq(x, -1) { do invoke py.fail(\"ValueError\", \"math domain error\") }\n",
                ),
                "atanh" => Some(
                    "if eq(abs(x), 1) { do invoke py.fail(\"ValueError\", \"math domain error\") }\n",
                ),
                "gamma" | "lgamma" => Some(
                    "if is_integer(x) { if le(x, 0) { do invoke py.fail(\"ValueError\", \"math domain error\") } }\n",
                ),
                "pow" => Some(
                    "if eq(x, 0) { if lt(y, 0) { do invoke py.fail(\"ValueError\", \"math domain error\") } }\n",
                ),
                _ => None,
            };
            if let Some(pole) = pole {
                source.push_str(pole);
            }
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
        } else {
            source.push_str(function.body);
        }
        source.push_str("\n}\n");
    }
    for module in ["math", "json"] {
        source.push_str(&format!("\nfunction py.module.{module}() -> Any\nkernel 1\ncharge 64\nbody {{\nreturn {{__py_module: {module:?}"));
        if module == "math" {
            source.push_str(", pi: 3.141592653589793, e: 2.718281828459045, tau: 6.283185307179586, inf: inf, nan: nan");
        }
        for function in FUNCTIONS
            .iter()
            .filter(|function| function.module == module)
        {
            let params = (0..function.params.len())
                .map(|index| format!("argument{index}"))
                .collect::<Vec<_>>()
                .join(", ");
            let passed = if module == "json" && function.name == "dumps" {
                format!(
                    "argument0, ({})",
                    (1..function.params.len())
                        .map(|index| format!("argument{index}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            } else {
                params.clone()
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
