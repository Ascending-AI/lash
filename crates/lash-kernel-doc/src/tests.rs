//! Laws of the document crate. Each pins a named rule of
//! `docs/kernel/semantics.md`.

use std::collections::BTreeMap;

use crate::{
    Action, Atom, Callee, Document, Expr, Float, Formula, FunctionDefinition, FunctionId,
    FunctionRegistry, Implementation, InvalidReason, Literal, MAX_NESTING_DEPTH, Name, Node,
    NumberPolicy, ParseErrorReason, RegistryError, StatementForm, Stmt, ValidatedFunctions,
    parse_definition, parse_document, print_definition, print_document, validate_document,
};

/// A definition named `name` taking one `Any`: native when `native`, with a
/// one-statement kernel body otherwise.
fn definition(name: &str, native: bool) -> FunctionDefinition {
    let text = if native {
        format!("function {name}(x: Any) -> Any\nkernel 1\ncharge 1\nnative\n")
    } else {
        format!("function {name}(x: Any) -> Any\nkernel 1\ncharge 1\nbody {{ return x }}\n")
    };
    parse_definition(&text).unwrap()
}

/// A catalog holding one native function, `num.neg`, and one with only a
/// kernel-code body, `helper.twice`, and the header that names both.
fn catalog() -> (BTreeMap<FunctionId, FunctionDefinition>, String) {
    let mut catalog = BTreeMap::new();
    let mut header = String::from("kernel 1\nnumbers float\neffect fetch(url: Text) -> Any\n");
    for (name, native) in [("num.neg", true), ("helper.twice", false)] {
        let definition = definition(name, native);
        let function = definition.identity().unwrap();
        header.push_str(&format!("use {name} = @{function}\n"));
        catalog.insert(function, definition);
    }
    (catalog, header)
}

/// Documents that together use every form, every literal, every type and
/// every way of naming a thing (`K-TEXT-002`).
fn every_form_corpus() -> Vec<String> {
    let (_, header) = catalog();
    let id_a = "a".repeat(64);
    let id_b = "b".repeat(64);
    vec![
        // Data and binding, control, functions, terminals.
        format!(
            r#"{header}
entry start(count: Int, label?: Text) -> List(Text)
private t1, `let`

fn start(count, label) {{
  let xs = [1, -2, 3.5, -0.0, nan, inf, -inf, 1e21, 1.5e-7, 123456789012345678901234567890]
  let t = (xs, "a\n\"b\"\t\u{{1f600}}\\", b"00ff", null, absent, true, false)
  let one = (t,)
  let none = ()
  let m = map{{"k": 1, (1, "two"): xs}}
  let s = set{{1, 2}}
  let r = {{name: "n", "odd key": {{}}, let: 1}}
  set r.name = r."odd key"
  set xs[0] = m["k"]
  set t1 = num.neg(num.neg(xs[1]))
  remove m["k"]
  remove r.name
  if true {{
    let `let` = 1
  }} else {{
    while false {{
      break
    }}
  }}
  for x in xs {{
    if x {{
      continue
    }}
  }}
  try {{
    throw {{kind: "boom"}}
  }} catch e {{
    print e
  }} finally {{
    print "done"
  }}
  try {{}} finally {{}}
  try {{}}
  return xs
}}

main {{
  let f = fn(a, b) {{
    let g = fn() {{
      return a
    }}
    return g
  }}
  let now = clock
  let roll = random
  let seen = read(f, {{path: ["a", 0]}})
  finish seen
  fail "unreachable"
}}
"#
        ),
        // Waits, tasks, every callee, every join mode.
        format!(
            r#"{header}
fn worker(url) {{
  let page = perform fetch(url) as Record{{status: Int, body?: Text, ..Any}}
  do sleep 10
  do yield
  return page
}}

main {{
  let h = spawn call worker("https://example.test")
  let g = fn(x) {{}}
  let h2 = spawn apply g(h)
  let h3 = spawn invoke helper.twice(1.5)
  let hs = [h, h2, h3]
  let first = join h
  let `all` = join all hs
  let outcomes = join settled hs
  let winner = join race hs
  let some = join any hs
  do join `all`
  do cancel h
  let twice = invoke helper.twice(first)
  let again = call worker(&worker)
  let applied = apply g(null, absent, true, -1, 2.5, "t", b"")
  do perform fetch("x") as Union(Null, Number, Tuple(Bool, Bytes), Map(Text, Set(Timestamp)), Enum("a", "b"), Task(Error), Handle("db"), Fn(a: Int, b?: Float) -> Absent, Record{{}}, Record{{..Text}})
  return null
}}
"#
        ),
        // Names that need quoting, calls by identity, a function two `use`
        // lines name, and library names spelled like keywords.
        format!(
            r#"kernel 1
numbers by_spelling
use join.all = @{id_a}
use shared.name = @{id_a_alt}
use shared.name = @{id_b}
use read = @{id_read}

fn `two words`(`a b`, `fn`) {{
  return `a b`
}}

main {{
  let x = join.all(1, @{id_b}(2), @{id_read}(3))
  let y = call `two words`(x, `x`)
  let z = invoke @{id_b}(y)
  let w = &`two words`
  let v = x.all.y[join.all(x)]."a b"
  return 1.f
}}
"#,
            id_a_alt = "c".repeat(64),
            id_read = "d".repeat(64),
        ),
    ]
}

/// `K-TEXT-002`: printing a document and parsing the text gives the
/// document back, and the printed text is a fixed point.
#[test]
fn kernel_text_round_trips_documents_that_use_every_form() {
    let mut forms = std::collections::BTreeSet::new();
    for text in every_form_corpus() {
        let document = parse_document(&text).unwrap_or_else(|error| panic!("{error}\n{text}"));
        let printed = print_document(&document);
        let reparsed =
            parse_document(&printed).unwrap_or_else(|error| panic!("{error}\n{printed}"));
        assert_eq!(reparsed, document, "{printed}");
        assert_eq!(print_document(&reparsed), printed);
        // The JSON encoding is the same document (`K-DOC-004`).
        let json = document.to_json().unwrap();
        assert_eq!(Document::from_json(&json).unwrap(), document);
        assert_eq!(
            Document::from_json(&json).unwrap().identity().unwrap(),
            document.identity().unwrap()
        );
        for body in document
            .functions
            .values()
            .map(|function| &function.body)
            .chain([&document.main])
        {
            collect_forms(Node::Block(body), &mut forms);
        }
    }
    // The corpus is only a proof if nothing is missing from it.
    let expected = [
        "action:call",
        "action:cancel",
        "action:join",
        "action:join_many",
        "action:perform",
        "action:sleep",
        "action:spawn",
        "action:yield",
        "callee:declared",
        "callee:library",
        "callee:value",
        "expr:call",
        "expr:clock",
        "expr:closure",
        "expr:field",
        "expr:index",
        "expr:list",
        "expr:literal",
        "expr:map",
        "expr:random",
        "expr:read",
        "expr:record",
        "expr:set",
        "expr:tuple",
        "expr:variable",
        "literal:absent",
        "literal:bool",
        "literal:bytes",
        "literal:float",
        "literal:function",
        "literal:int",
        "literal:null",
        "literal:text",
        "stmt:assign",
        "stmt:break",
        "stmt:continue",
        "stmt:do",
        "stmt:fail",
        "stmt:finish",
        "stmt:for",
        "stmt:if",
        "stmt:let",
        "stmt:print",
        "stmt:remove",
        "stmt:return",
        "stmt:throw",
        "stmt:try",
        "stmt:while",
    ];
    assert_eq!(forms.into_iter().collect::<Vec<_>>(), expected);
}

fn collect_forms(node: Node<'_>, forms: &mut std::collections::BTreeSet<&'static str>) {
    let literal = |forms: &mut std::collections::BTreeSet<&'static str>, literal: &Literal| {
        forms.insert(match literal {
            Literal::Null => "literal:null",
            Literal::Absent => "literal:absent",
            Literal::Bool(_) => "literal:bool",
            Literal::Int(_) => "literal:int",
            Literal::Float(_) => "literal:float",
            Literal::Text(_) => "literal:text",
            Literal::Bytes(_) => "literal:bytes",
            Literal::Function(_) => "literal:function",
        });
    };
    match node {
        Node::Block(_) => {}
        Node::Stmt(stmt) => {
            forms.insert(match stmt {
                Stmt::Let { .. } => "stmt:let",
                Stmt::Assign { .. } => "stmt:assign",
                Stmt::Remove { .. } => "stmt:remove",
                Stmt::Do { .. } => "stmt:do",
                Stmt::If { .. } => "stmt:if",
                Stmt::For { .. } => "stmt:for",
                Stmt::While { .. } => "stmt:while",
                Stmt::Break => "stmt:break",
                Stmt::Continue => "stmt:continue",
                Stmt::Return { .. } => "stmt:return",
                Stmt::Try(_) => "stmt:try",
                Stmt::Throw { .. } => "stmt:throw",
                Stmt::Print { .. } => "stmt:print",
                Stmt::Finish { .. } => "stmt:finish",
                Stmt::Fail { .. } => "stmt:fail",
            });
        }
        Node::Action(action) => {
            let (name, callee, atoms): (_, Option<&Callee>, &[Atom]) = match action {
                Action::Call { callee, args } => ("action:call", Some(callee), args),
                Action::Spawn { callee, args } => ("action:spawn", Some(callee), args),
                Action::Perform { args, .. } => ("action:perform", None, args),
                Action::Sleep { duration } => {
                    ("action:sleep", None, std::slice::from_ref(duration))
                }
                Action::Join { task } => ("action:join", None, std::slice::from_ref(task)),
                Action::JoinMany { tasks, .. } => {
                    ("action:join_many", None, std::slice::from_ref(tasks))
                }
                Action::Yield => ("action:yield", None, &[]),
                Action::Cancel { task } => ("action:cancel", None, std::slice::from_ref(task)),
            };
            forms.insert(name);
            if let Some(callee) = callee {
                forms.insert(match callee {
                    Callee::Declared(_) => "callee:declared",
                    Callee::Value(_) => "callee:value",
                    Callee::Library(_) => "callee:library",
                });
            }
            for atom in atoms {
                if let Atom::Literal(value) = atom {
                    literal(forms, value);
                }
            }
        }
        Node::Expr(expr) => {
            if let Expr::Literal(value) = expr {
                literal(forms, value);
            }
            forms.insert(match expr {
                Expr::Literal(_) => "expr:literal",
                Expr::Variable(_) => "expr:variable",
                Expr::Tuple(_) => "expr:tuple",
                Expr::List(_) => "expr:list",
                Expr::Map(_) => "expr:map",
                Expr::Set(_) => "expr:set",
                Expr::Record(_) => "expr:record",
                Expr::Member(member) => match member.as_ref() {
                    crate::Member::Field { .. } => "expr:field",
                    crate::Member::Index { .. } => "expr:index",
                },
                Expr::Closure(_) => "expr:closure",
                Expr::Call { .. } => "expr:call",
                Expr::Clock => "expr:clock",
                Expr::Random => "expr:random",
                Expr::Read(_) => "expr:read",
            });
        }
    }
    for child in node.children() {
        collect_forms(child, forms);
    }
}

/// `K-TEXT-002` for definitions: every implementation kind, a guard, error
/// kinds and every formula.
#[test]
fn kernel_text_round_trips_definitions() {
    let (_, header) = catalog();
    let uses: String = header.lines().filter(|line| line.starts_with("use ")).fold(
        String::new(),
        |mut uses, line| {
            uses.push_str(line);
            uses.push('\n');
            uses
        },
    );
    let texts = [
        "function regex.ecma.exec(pattern: Text, input: Text, `result`?: Int) -> Union(Null, Record{index: Int})\n\
         kernel 1\nerrors \"regex.syntax\", \"type_error\"\n\
         charge sum(1, size(pattern), product(2, deep(input)), max(magnitude(`result`), 3), min(size(result), 4))\n\
         guard \"backtracking step\" product(1000, size(input))\nnative\n"
            .to_string(),
        format!(
            "function list.map(items: List(Any), f: Fn(item: Any) -> Any) -> List(Any)\nkernel 1\n\
             charge sum(1, size(items))\n{uses}body {{\n  let out = []\n  for item in items {{\n    \
             let mapped = apply f(item)\n    set out[num.neg(0)] = mapped\n  }}\n  return out\n}}\n"
        ),
        format!(
            "function num.double_neg(x: Number) -> Number\nkernel 1\ncharge 1\nnative\n{uses}\
             body {{\n  return num.neg(num.neg(x))\n}}\n"
        ),
    ];
    let kinds: Vec<&str> = texts
        .iter()
        .map(|text| {
            let definition =
                parse_definition(text).unwrap_or_else(|error| panic!("{error}\n{text}"));
            let printed = print_definition(&definition);
            assert_eq!(parse_definition(&printed).unwrap(), definition, "{printed}");
            let json = definition.to_json().unwrap();
            assert_eq!(FunctionDefinition::from_json(&json).unwrap(), definition);
            match definition.implementation {
                Implementation::Native => "native",
                Implementation::Body(_) => "body",
                Implementation::Both(_) => "both",
            }
        })
        .collect();
    assert_eq!(kinds, ["native", "body", "both"]);
}

fn main_of(statements: &str) -> String {
    let (_, header) = catalog();
    format!("{header}fn declared(x) {{ return x }}\nmain {{\n{statements}\n}}\n")
}

/// `K-STMT-002`: a call inside an expression is admitted when its callee
/// has a native implementation, and refused, naming the node, when it has
/// only a kernel-code body.
#[test]
fn statement_rule_admits_a_nested_native_call_and_refuses_a_nested_body_only_call() {
    let (catalog, _) = catalog();
    let admitted = parse_document(&main_of("let y = num.neg(num.neg(1))")).unwrap();
    assert_eq!(validate_document(&admitted, &catalog), Ok(()));

    let refused = parse_document(&main_of("let x = 1\nlet y = num.neg(helper.twice(x))")).unwrap();
    let error = validate_document(&refused, &catalog).unwrap_err();
    assert!(
        matches!(error.reason.as_ref(), InvalidReason::CallNotNative { name, .. } if name.as_str() == "helper.twice"),
        "{error}"
    );
    // The second statement of `main`, its right-hand side, that call's first
    // argument.
    let site = error.site.unwrap();
    assert_eq!(site.to_string(), "main/1/0/0");
    assert!(matches!(
        refused.node(&site),
        Some(Node::Expr(Expr::Call { .. }))
    ));
    // The same call as its own statement is admitted.
    let hoisted = parse_document(&main_of(
        "let x = 1\nlet t = invoke helper.twice(x)\nlet y = num.neg(t)",
    ))
    .unwrap();
    assert_eq!(validate_document(&hoisted, &catalog), Ok(()));
}

/// `K-STMT-001`: `perform`, `spawn` and every other statement form have no
/// place inside an expression. Kernel text refuses each with a typed error
/// at the form; the tree has no node that could hold one.
#[test]
fn statement_rule_refuses_statement_forms_nested_in_an_expression() {
    let cases = [
        (
            "let y = num.neg(perform fetch(\"u\") as Any)",
            StatementForm::Perform,
            (8, 17),
        ),
        (
            "let y = [spawn call declared(1)]",
            StatementForm::Spawn,
            (8, 10),
        ),
        (
            "let y = num.neg(call declared(1))",
            StatementForm::Call,
            (8, 17),
        ),
        (
            "let h = 1\nlet y = {v: join h}",
            StatementForm::Join,
            (9, 13),
        ),
        ("let y = (sleep 1,)", StatementForm::Sleep, (8, 10)),
        ("let y = map{1: yield}", StatementForm::Yield, (8, 16)),
        (
            "let h = 1\nlet y = set{cancel h}",
            StatementForm::Cancel,
            (9, 13),
        ),
        (
            "let y = call declared(spawn call declared(1))",
            StatementForm::Spawn,
            (8, 23),
        ),
    ];
    for (statements, form, (line, column)) in cases {
        let error = parse_document(&main_of(statements)).unwrap_err();
        assert_eq!(
            (error.reason.clone(), error.line, error.column),
            (ParseErrorReason::StatementForm { form }, line, column),
            "{statements}: {error}"
        );
    }
    // An argument of a statement form is a variable or a literal.
    let error = parse_document(&main_of("let y = call declared(num.neg(1))")).unwrap_err();
    assert_eq!(error.reason, ParseErrorReason::ArgumentNotAtom);
    // The JSON encoding has no spelling for a nested wait either.
    let nested = r#"{"manifest":{"kernel":1,"numbers":"float"},"main":[{"let":{"name":"y","value":{"expr":{"list":[{"spawn":{"callee":{"declared":"f"},"args":[]}}]}}}}]}"#;
    assert!(Document::from_json(nested).is_err());
}

/// `K-ID-001`: the identity is over behaviour alone, and over all of it.
#[test]
fn identity_is_stable_across_encodings_and_moves_with_behaviour() {
    let document = parse_document(&main_of("let y = 1")).unwrap();
    let identity = document.identity().unwrap();
    // White space, comments and the order of top-level items are not
    // behaviour.
    let respelled = main_of("# a comment\n   let   y =\n 1");
    assert_eq!(
        parse_document(&respelled).unwrap().identity().unwrap(),
        identity
    );
    // A different literal is.
    let changed = parse_document(&main_of("let y = 1.0")).unwrap();
    assert_ne!(changed.identity().unwrap(), identity);
    // The canonical form is pinned: a reader in another language must
    // reach this digest for this document.
    let pinned = Document::new(
        NumberPolicy::Float,
        vec![Stmt::Return {
            value: Expr::Literal(Literal::Float(Float::new(-0.0))),
        }],
    );
    assert_eq!(
        String::from_utf8(crate::canonical::canonical_bytes(&pinned).unwrap()).unwrap(),
        r#"{"main":[{"return":{"value":{"literal":{"float":"-0.0"}}}}],"manifest":{"kernel":1,"numbers":"float"}}"#
    );
}

/// `K-VAL-031`: a float's text is the shortest digits that read back, in
/// the pinned layout.
#[test]
fn float_text_is_shortest_round_trip_in_the_pinned_layout() {
    let cases = [
        (0.0, "0.0"),
        (-0.0, "-0.0"),
        (1.0, "1.0"),
        (-1.5, "-1.5"),
        (0.1, "0.1"),
        (0.1 + 0.2, "0.30000000000000004"),
        (1e-4, "0.0001"),
        (9.999e-5, "9.999e-5"),
        (1e15, "1000000000000000.0"),
        (9007199254740993.0, "9007199254740992.0"),
        (1e16, "1e16"),
        (1.5e300, "1.5e300"),
        (5e-324, "5e-324"),
        (f64::MAX, "1.7976931348623157e308"),
        (f64::INFINITY, "inf"),
        (f64::NEG_INFINITY, "-inf"),
        (f64::NAN, "nan"),
    ];
    for (value, text) in cases {
        let float = Float::new(value);
        assert_eq!(float.to_string(), text);
        assert_eq!(Float::parse(text).unwrap(), float);
    }
    // Only the canonical spelling is stored.
    for text in [
        "1", "1.", "01.0", "1.50", "1e5", "+1.0", "NaN", "1E16", "1.0e16",
    ] {
        assert!(Float::parse(text).is_err(), "{text}");
    }
}

/// `K-DOC-006`: the nesting limit is the same for kernel text, JSON and
/// validation, and the deepest admitted document passes through all three.
#[test]
fn the_nesting_limit_holds_in_text_json_and_validation() {
    // A block is one level and each statement in it another, so `depth`
    // nested `if`s put the innermost statement 2 * depth + 2 levels down.
    let nested = |depth: usize| {
        let mut text = String::from("kernel 1\nnumbers float\nmain {\n");
        for _ in 0..depth {
            text.push_str("if true {\n");
        }
        text.push_str("break\n");
        for _ in 0..depth {
            text.push_str("}\n");
        }
        text.push_str("}\n");
        text
    };
    let empty = BTreeMap::new();
    let deepest = (MAX_NESTING_DEPTH - 2) / 2;
    let mut document = parse_document(&nested(deepest)).unwrap();
    // `break` outside a loop is the only fault; depth is not one.
    assert!(matches!(
        *validate_document(&document, &empty).unwrap_err().reason,
        InvalidReason::LoopControlOutsideLoop { .. }
    ));
    assert_eq!(
        parse_document(&print_document(&document)).unwrap(),
        document
    );
    assert_eq!(
        Document::from_json(&document.to_json().unwrap()).unwrap(),
        document
    );

    assert!(matches!(
        parse_document(&nested(deepest + 1)).unwrap_err().reason,
        ParseErrorReason::TooDeep { .. }
    ));
    // A tree built in memory past the limit is refused by validation.
    document.main = vec![Stmt::If {
        condition: Expr::Literal(Literal::Bool(true)),
        then_block: std::mem::take(&mut document.main),
        else_block: Vec::new(),
    }];
    assert!(matches!(
        *validate_document(&document, &empty).unwrap_err().reason,
        InvalidReason::TooDeep { .. }
    ));
}

/// `K-LIB-003`, `K-LIB-004`: a function with a native implementation takes
/// no function, and a body beside a native implementation cannot wait.
#[test]
fn a_native_function_takes_no_function_and_its_body_cannot_wait() {
    let (catalog, header) = catalog();
    let uses: String = header
        .lines()
        .filter(|line| line.starts_with("use "))
        .map(|line| format!("{line}\n"))
        .collect();
    let takes_function = parse_definition(
        "function list.each(items: List(Any), f: Fn(x: Any) -> Any) -> Null\nkernel 1\ncharge 1\nnative\n",
    )
    .unwrap();
    assert!(matches!(
        *crate::validate_definition(&takes_function, &catalog).unwrap_err().reason,
        InvalidReason::NativeTakesFunction { name } if name == Name::new("f")
    ));
    let waits = parse_definition(&format!(
        "function slow(x: Any) -> Any\nkernel 1\ncharge 1\nnative\n{uses}body {{\n  do sleep 1\n  return x\n}}\n"
    ))
    .unwrap();
    assert_eq!(
        *crate::validate_definition(&waits, &catalog)
            .unwrap_err()
            .reason,
        InvalidReason::NativeBodyMayWait {
            form: StatementForm::Sleep
        }
    );
    let calls_native = parse_definition(&format!(
        "function fine(x: Any) -> Any\nkernel 1\ncharge 1\nnative\n{uses}body {{\n  let y = invoke num.neg(x)\n  return y\n}}\n"
    ))
    .unwrap();
    assert_eq!(crate::validate_definition(&calls_native, &catalog), Ok(()));
}

/// `K-CHG-003`: formula arithmetic saturates.
#[test]
fn a_formula_saturates() {
    let formula = Formula::Product(vec![Formula::Constant(u64::MAX), Formula::Constant(2)]);
    assert_eq!(formula.evaluate(&mut |_, _| 0), u64::MAX);
}

/// Functions validated against a registry's contents join, without being
/// validated again, only a registry holding exactly those contents, under
/// the identities validation gave them (FIG-5796).
#[test]
fn validated_functions_join_only_the_registry_they_were_validated_against() {
    let holding = |name: &str| {
        let mut registry = FunctionRegistry::new();
        registry.register(definition(name, false), None).unwrap();
        registry
    };
    let mut built = holding("helper.base");
    let validated = built
        .validate_functions(vec![definition("helper.joined", false)])
        .unwrap();
    let validated = ValidatedFunctions::from_json(&validated.to_json().unwrap()).unwrap();
    let mut same = holding("helper.base");
    same.register_validated(validated.clone()).unwrap();
    assert!(
        same.iter()
            .map(|(id, _)| id)
            .eq(built.iter().map(|(id, _)| id))
    );
    for mut other in [holding("helper.other"), FunctionRegistry::new()] {
        let held = other.len();
        assert!(matches!(
            other.register_validated(validated.clone()),
            Err(RegistryError::OtherBasis { .. })
        ));
        assert_eq!(other.len(), held, "nothing joins another registry");
    }
}

/// `K-LIB-011`: a native implementation states its version, the first when
/// it states none. A definition that states none has the identity it had
/// before versions were stated; the next version is a new function, written
/// `native <version>`, and a definition with no native implementation, or
/// a version 0, is refused.
#[test]
fn a_native_version_is_part_of_the_identity_and_only_a_native_states_one() {
    let first = definition("num.twice", true);
    assert_eq!(first.native_version, crate::FIRST_NATIVE_VERSION);
    assert!(
        !first.to_json().unwrap().contains("native_version"),
        "the first version is not written"
    );
    let text = "function num.twice(x: Any) -> Any\nkernel 1\ncharge 1\nnative 2\n";
    let second = parse_definition(text).unwrap();
    assert_eq!(second.native_version, 2);
    assert_eq!(print_definition(&second), text);
    assert_ne!(second.identity().unwrap(), first.identity().unwrap());
    let empty = FunctionRegistry::new();
    crate::validate_definition(&second, &empty).expect("a native states its version");
    let body = FunctionDefinition {
        native_version: 2,
        ..definition("helper.same", false)
    };
    assert!(matches!(
        crate::validate_definition(&body, &empty).map_err(|invalid| *invalid.reason),
        Err(InvalidReason::NativeVersionWithoutNative { version: 2 })
    ));
    let zero = FunctionDefinition {
        native_version: 0,
        ..first
    };
    assert!(matches!(
        crate::validate_definition(&zero, &empty).map_err(|invalid| *invalid.reason),
        Err(InvalidReason::NativeVersionZero)
    ));
}
