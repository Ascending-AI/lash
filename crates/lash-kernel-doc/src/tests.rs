//! Laws of the document crate. Each pins a named rule of
//! `docs/kernel/semantics.md`.

use std::collections::BTreeMap;

use crate::{
    Action, AssignOrder, Atom, Callee, Document, Expr, Float, Formula, FunctionDefinition,
    FunctionId, FunctionRegistry, Implementation, InvalidReason, Literal, MAX_NESTING_DEPTH, Name,
    Node, NumberPolicy, ParseErrorReason, Place, RegistryError, ReleasedFunctions, Rhs,
    StatementForm, Stmt, parse_definition, parse_document, print_definition, print_document,
    validate_document,
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
pub(crate) fn catalog() -> (BTreeMap<FunctionId, FunctionDefinition>, String) {
    let mut catalog = BTreeMap::new();
    let mut header = String::from("kernel 1\nnumbers float\neffect fetch(url: Text) -> Any\n");
    for (name, native) in [("num.neg", true), ("helper.twice", false)] {
        let definition = definition(name, native);
        let function = definition.identity();
        header.push_str(&format!("use {name} = @{function}\n"));
        catalog.insert(function, definition);
    }
    (catalog, header)
}

/// Documents that together use every form, every literal, every type and
/// every way of naming a thing (`K-TEXT-002`).
pub(crate) fn every_form_corpus() -> Vec<String> {
    let (_, header) = catalog();
    let id_a = "a".repeat(64);
    let id_b = "b".repeat(64);
    vec![
        // Data and binding, control, functions, terminals.
        format!(
            r#"{header}
entry start(count: Int, label?: Text) -> List(Text)
entry positional(value: Any) -> Any ts_direct(helper.twice)
entry packed(value: Any) -> Any ts_args(helper.twice, num.neg)
entry cleaned(value: Any) -> Any py_direct(helper.twice)
fn positional(value) {{ return value via helper.twice }}
fn packed(this, args) {{ return args }}
fn cleaned(value) {{ return value }}
private t1, `let`

fn start(count, label) {{
  let xs = [1, -2, 3.5, -0.0, nan, inf, -inf, 1e21, 1.5e-7, 123456789012345678901234567890]
  let t = (xs, "a\n\"b\"\t\u{{1f600}}\\", b"00ff", null, absent, true, false)
  let chosen = if(true, let(x = 7, x), 0)
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
  let later = perform fetch(url) as Any then yield
  do sleep 0 then yield
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
  let cancelled = cancel h
  let yielded = yield
  let slept = sleep 0
  let slept_later = sleep 0 then yield
  let twice = invoke helper.twice(first)
  let again = call worker(&worker)
  let applied = apply g(null, absent, true, -1, 2.5, "t", b"")
  let nested = [call worker([1][0]), apply g(1), invoke helper.twice(first), perform fetch("y") as Any, sleep(1), spawn call worker("z")]
  let waits = [perform fetch("y") as Any then yield, sleep(1) then yield, join(h), join all([h2, h3]), await([h][0]) via helper.twice]
  let awaited = await h via helper.twice
  set nested[0] = (sleep 5, spawn apply g(first))
  set [perform fetch("t") as Any, nested][1][call worker(1)] = 0
  set [perform fetch("t") as Any, nested][1][call worker(1)] = sleep 5 place first
  set nested[0].f = [1, perform fetch("v") as Any] place first
  while [perform fetch("w") as Any, false][1] {{
    break
  }}
  let direct = call worker([1][0])
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
            Document::from_json(&json).unwrap().identity(),
            document.identity()
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
        "action:await",
        "action:call",
        "action:cancel",
        "action:join",
        "action:join_many",
        "action:perform",
        "action:perform:then_yield",
        "action:sleep",
        "action:sleep:then_yield",
        "action:spawn",
        "action:yield",
        "callee:declared",
        "callee:library",
        "callee:value",
        "expr:act:await",
        "expr:act:call",
        "expr:act:cancel",
        "expr:act:join",
        "expr:act:join_many",
        "expr:act:perform",
        "expr:act:perform:then_yield",
        "expr:act:sleep",
        "expr:act:sleep:then_yield",
        "expr:act:spawn",
        "expr:act:yield",
        "expr:call",
        "expr:clock",
        "expr:closure",
        "expr:field",
        "expr:if",
        "expr:index",
        "expr:let",
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
        "stmt:assign:place_first",
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
        "stmt:return-via",
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
                Stmt::Assign {
                    order: AssignOrder::PlaceFirst,
                    ..
                } => "stmt:assign:place_first",
                Stmt::Assign { .. } => "stmt:assign",
                Stmt::Remove { .. } => "stmt:remove",
                Stmt::Do { .. } => "stmt:do",
                Stmt::If { .. } => "stmt:if",
                Stmt::For { .. } => "stmt:for",
                Stmt::While { .. } => "stmt:while",
                Stmt::Break => "stmt:break",
                Stmt::Continue => "stmt:continue",
                Stmt::ReturnVia { .. } => "stmt:return-via",
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
                Action::Perform {
                    args, then_yield, ..
                } => (
                    if *then_yield {
                        "action:perform:then_yield"
                    } else {
                        "action:perform"
                    },
                    None,
                    args,
                ),
                Action::Sleep {
                    duration,
                    then_yield,
                } => (
                    if *then_yield {
                        "action:sleep:then_yield"
                    } else {
                        "action:sleep"
                    },
                    None,
                    std::slice::from_ref(duration),
                ),
                Action::Await { value, .. } => ("action:await", None, std::slice::from_ref(value)),
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
                Expr::If { .. } => "expr:if",
                Expr::Let { .. } => "expr:let",
                Expr::Read(_) => "expr:read",
                Expr::Act(act) => match &act.kind {
                    crate::ActKind::Call(_) => "expr:act:call",
                    crate::ActKind::Perform {
                        then_yield: false, ..
                    } => "expr:act:perform",
                    crate::ActKind::Perform {
                        then_yield: true, ..
                    } => "expr:act:perform:then_yield",
                    crate::ActKind::Sleep { then_yield: false } => "expr:act:sleep",
                    crate::ActKind::Sleep { then_yield: true } => "expr:act:sleep:then_yield",
                    crate::ActKind::Spawn(_) => "expr:act:spawn",
                    crate::ActKind::Yield => "expr:act:yield",
                    crate::ActKind::Cancel => "expr:act:cancel",
                    crate::ActKind::Join => "expr:act:join",
                    crate::ActKind::JoinMany(_) => "expr:act:join_many",
                    crate::ActKind::Await(_) => "expr:act:await",
                },
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

/// `K-LIB-002`: a library function called by name inside an expression
/// states a native implementation; one with only a kernel-code body is
/// refused there, naming the call, and is called inside an expression with
/// `invoke`, a nested action (`K-EVAL-008`).
#[test]
fn a_body_only_library_function_is_invoked_inside_an_expression() {
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
    let invoked = parse_document(&main_of(
        "let x = 1\nlet y = num.neg(invoke helper.twice(x))",
    ))
    .unwrap();
    assert_eq!(validate_document(&invoked, &catalog), Ok(()));
}

/// `K-EVAL-003`, `K-DOC-005`: a place-first assignment writes a member
/// from an expression. Its text reads a whole-action value as the nested
/// action; a variable place, which evaluates nothing, and the statement's
/// own action, which no continuation holds the place across, are refused,
/// naming the node.
#[test]
fn a_place_first_assignment_writes_a_member_from_an_expression() {
    let (catalog, _) = catalog();
    let document = parse_document(&main_of(
        "let xs = [0]\nset xs[0] = call declared(1) place first",
    ))
    .unwrap();
    assert_eq!(validate_document(&document, &catalog), Ok(()));
    let Stmt::Assign { value, order, .. } = &document.main[1] else {
        panic!("{:?}", document.main[1]);
    };
    assert_eq!(*order, AssignOrder::PlaceFirst);
    assert!(matches!(value, Rhs::Expr(Expr::Act(_))), "{value:?}");

    let mut stated = document.clone();
    if let Stmt::Assign { value, .. } = &mut stated.main[1] {
        *value = Rhs::Action(Action::Call {
            callee: Callee::Declared(Name::new("declared")),
            args: vec![Atom::Literal(Literal::Int(1.into()))],
        });
    }
    let error = validate_document(&stated, &catalog).unwrap_err();
    assert!(
        matches!(
            error.reason.as_ref(),
            InvalidReason::PlaceFirstAssignment { .. }
        ),
        "{error}"
    );
    assert_eq!(error.site.unwrap().to_string(), "main/1/2");

    let mut variable = document;
    variable.main[1] = Stmt::Assign {
        place: Place::Variable(Name::new("xs")),
        value: Rhs::Expr(Expr::Literal(Literal::Null)),
        order: AssignOrder::PlaceFirst,
    };
    let error = validate_document(&variable, &catalog).unwrap_err();
    assert!(
        matches!(
            error.reason.as_ref(),
            InvalidReason::PlaceFirstAssignment { .. }
        ),
        "{error}"
    );
    assert_eq!(error.site.unwrap().to_string(), "main/1");
}

/// `K-ID-001`: the identity is over behaviour alone, and over all of it.
#[test]
fn identity_is_stable_across_encodings_and_moves_with_behaviour() {
    let document = parse_document(&main_of("let y = 1")).unwrap();
    let identity = document.identity();
    // White space, comments and the order of top-level items are not
    // behaviour.
    let respelled = main_of("# a comment\n   let   y =\n 1");
    assert_eq!(parse_document(&respelled).unwrap().identity(), identity);
    // A different literal is.
    let changed = parse_document(&main_of("let y = 1.0")).unwrap();
    assert_ne!(changed.identity(), identity);
    // The canonical form is pinned (`K-ID-003`): the document's logical
    // token body, and its identity that body's logical digest.
    let pinned = Document::new(
        NumberPolicy::Float,
        vec![Stmt::Return {
            value: Expr::Literal(Literal::Float(Float::new(-0.0))),
        }],
    );
    let bytes = pinned.canonical_bytes();
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(
        hex,
        "0905090403010a000a000c00000a000a00090009010c09010c00010c0401060000000000000080"
    );
    assert_eq!(
        crate::graph_tokens::decode_body::<Document>(&bytes).unwrap(),
        pinned
    );
    assert_eq!(
        pinned.identity().as_bytes(),
        &crate::graph_tokens::logical_digest(&bytes)
    );
    assert_eq!(
        pinned.identity().to_string(),
        "423297a9161481df576e8d264afe1ea8546ba58fb1b9773b1ce6d1a02245a421"
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
/// no function, and a body beside a native implementation cannot wait
/// itself: it calls only functions that state a native implementation, so
/// its own work is covered by its formula (`K-CHG-007`), and applies only
/// closures, which run as ordinary code.
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
    let calls_helper = parse_definition(&format!(
        "function busy(x: Any) -> Any\nkernel 1\ncharge 1\nnative\n{uses}body {{\n  let y = invoke helper.twice(x)\n  return y\n}}\n"
    ))
    .unwrap();
    assert_eq!(
        *crate::validate_definition(&calls_helper, &catalog)
            .unwrap_err()
            .reason,
        InvalidReason::NativeBodyMayWait {
            form: StatementForm::Call
        }
    );
    let calls_native = parse_definition(&format!(
        "function fine(x: Any) -> Any\nkernel 1\ncharge 1\nnative\n{uses}body {{\n  let y = invoke num.neg(x)\n  return y\n}}\n"
    ))
    .unwrap();
    assert_eq!(crate::validate_definition(&calls_native, &catalog), Ok(()));
    let applies = parse_definition(&format!(
        "function convert(x: Any) -> Any\nkernel 1\ncharge 1\nnative\n{uses}body {{\n  let y = apply x(1)\n  let g = fn(v) {{ return num.neg(v) }}\n  let z = apply g(y)\n  return z\n}}\n"
    ))
    .unwrap();
    assert_eq!(crate::validate_definition(&applies, &catalog), Ok(()));
    let closure_waits = parse_definition(&format!(
        "function later(x: Any) -> Any\nkernel 1\ncharge 1\nnative\n{uses}body {{\n  let f = fn() {{ do sleep 1 return x }}\n  let y = apply f()\n  return y\n}}\n"
    ))
    .unwrap();
    assert_eq!(
        *crate::validate_definition(&closure_waits, &catalog)
            .unwrap_err()
            .reason,
        InvalidReason::NativeBodyMayWait {
            form: StatementForm::Sleep
        }
    );
}

/// `K-CHG-003`: formula arithmetic saturates.
#[test]
fn a_formula_saturates() {
    let formula = Formula::Product(vec![Formula::Constant(u64::MAX), Formula::Constant(2)]);
    assert_eq!(formula.evaluate(&mut |_, _| 0), u64::MAX);
}

/// Functions validated against a registry's contents join, without being
/// validated again, only a registry holding every one of those contents,
/// under the identities validation gave them (FIG-5796). A registry that
/// holds more, as a worker with a second dialect does, is joined too
/// (FIG-5990).
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
    let image: &'static [u8] = Vec::leak(validated.to_image(&|_| None));
    let validated = ReleasedFunctions::read(image).unwrap();
    let mut same = holding("helper.base");
    same.register_released(validated).unwrap();
    assert!(
        same.iter()
            .map(|(id, _)| id)
            .eq(built.iter().map(|(id, _)| id))
    );
    let mut wider = holding("helper.base");
    wider
        .register(definition("helper.other", false), None)
        .unwrap();
    wider.register_released(validated).unwrap();
    assert_eq!(wider.len(), built.len() + 1);
    for mut other in [holding("helper.other"), FunctionRegistry::new()] {
        let held = other.len();
        assert!(matches!(
            other.register_released(validated),
            Err(RegistryError::OtherBasis { .. })
        ));
        assert_eq!(other.len(), held, "nothing joins another registry");
    }
}

/// FIG-6843: a registry takes a build's release image in place. Each
/// function's name, implementation, kernel version, listed callees and
/// census answer without decoding its definition; reading the definition
/// decodes it once, for every clone of the registry; and the definition is
/// the one validated.
#[test]
fn a_release_image_registers_without_decoding_a_definition() {
    let mut built = FunctionRegistry::declarations();
    let base = built
        .register(definition("helper.base", true), None)
        .unwrap();
    let calling = parse_definition(&format!(
        "function helper.calling(x: Any) -> Any\nkernel 1\ncharge 1\nuse helper.base = @{base}\n\
         body {{ let y = helper.base(x) return y }}\n"
    ))
    .unwrap();
    let validated = built
        .validate_functions(vec![definition("helper.joined", false), calling.clone()])
        .unwrap();
    let census = crate::BodyCensus {
        codes: 1,
        blocks: 2,
        stmts: 3,
        sites: 4,
        closures: true,
        occurrences: vec![vec![0, 1], vec![]],
    };
    let image: &'static [u8] = Vec::leak(validated.to_image(&|definition| {
        (definition.name.as_str() == "helper.calling").then(|| census.clone())
    }));
    let image = ReleasedFunctions::read(image).unwrap();
    let decoded = || crate::released::DECODED.with(std::cell::Cell::get);
    let before = decoded();
    let mut registry = FunctionRegistry::declarations();
    registry
        .register(definition("helper.base", true), None)
        .unwrap();
    registry.register_released(image).unwrap();
    let calling_id = calling.identity();
    let held = registry.get(&calling_id).unwrap();
    assert!(held.released());
    assert_eq!(held.name(), "helper.calling");
    assert!(held.has_body() && !held.states_native());
    assert_eq!(held.kernel(), 1);
    assert_eq!(
        held.listed(),
        vec![(base, crate::FunctionName::new("helper.base").unwrap())]
    );
    assert_eq!(held.census(), Some(census));
    assert_eq!(
        registry.named("helper.joined").collect::<Vec<_>>(),
        vec![definition("helper.joined", false).identity()]
    );
    assert_eq!(
        decoded(),
        before,
        "registering and reading facts decode nothing"
    );
    let clone = registry.clone();
    assert_eq!(**registry.get(&calling_id).unwrap().definition(), calling);
    assert_eq!(**clone.get(&calling_id).unwrap().definition(), calling);
    assert_eq!(
        decoded(),
        before + 1,
        "clones share the one decoded definition"
    );
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
    assert_ne!(second.identity(), first.identity());
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

/// FIG-6255: registration takes a body's identity once, including validation's sites.
#[test]
fn registration_identifies_each_immutable_definition_once() {
    let definition = crate::parse_definition(
        "function helper() -> Int\nkernel 1\ncharge 1\nbody { return 1 }\n",
    )
    .unwrap();
    let before = crate::function::IDENTITY_CALLS.with(std::cell::Cell::get);
    crate::FunctionRegistry::declarations()
        .register(definition, None)
        .unwrap();
    let after = crate::function::IDENTITY_CALLS.with(std::cell::Cell::get);
    assert_eq!(after - before, 1);
}

/// DECLARATIONS-RETAIN-ONCE (FIG-6457): a document's declarations drawn from a
/// registry are checked against the identities registration took, never by
/// identifying each definition again, and carry the JSON registration
/// encoded once.
#[test]
fn registered_declarations_check_without_identifying_again() {
    let mut registry = FunctionRegistry::declarations();
    let mut header = String::from("kernel 1\nnumbers float\n");
    for name in ["helper.one", "helper.two", "helper.three"] {
        let function = registry.register(definition(name, false), None).unwrap();
        header.push_str(&format!("use {name} = @{function}\n"));
    }
    let document = parse_document(&format!("{header}main {{ return 1 }}\n")).unwrap();
    assert_eq!(document.manifest.functions.len(), 3);
    let before = crate::function::IDENTITY_CALLS.with(std::cell::Cell::get);
    let declarations = crate::required_declarations(&document, &registry).unwrap();
    let first = crate::CheckedDeclarations::required(&document, &registry).unwrap();
    let second = crate::CheckedDeclarations::required(&document, &registry).unwrap();
    first.check(&document).unwrap();
    let after = crate::function::IDENTITY_CALLS.with(std::cell::Cell::get);
    assert_eq!(declarations.len(), 3);
    assert_eq!(
        after - before,
        0,
        "registered declarations are identified again"
    );
    let (first, second) = (first.images(), second.images());
    assert_eq!(first.len(), 3);
    assert!(
        first
            .iter()
            .zip(&second)
            .all(|((_, one), (_, two))| std::sync::Arc::ptr_eq(one, two)),
        "a registered declaration is encoded once"
    );
    for (function, image) in &first {
        let definition = registry.get(function).unwrap().definition();
        assert_eq!(&**image, crate::graph_tokens::encode_body(&**definition));
    }
}

/// K-CHG-003, FIG-6528: integer ceil_log2 is exact at every power of two,
/// including the unsigned limit, and survives text, JSON and graph encoding.
#[test]
fn logarithmic_charge_rounds_up_and_round_trips() {
    let mut cases: Vec<(u64, u32)> = vec![(0, 0), (1, 0), (u64::MAX, 64)];
    for bit in 1..64 {
        let power = 1u64 << bit;
        cases.extend([
            (power - 1, if bit == 1 { 0 } else { bit }),
            (power, bit),
            (power + 1, bit + 1),
        ]);
    }
    for (amount, expected) in cases {
        let formula = Formula::CeilLog2(Box::new(Formula::Constant(amount)));
        assert_eq!(
            formula.evaluate(&mut |_, _| unreachable!()),
            u64::from(expected),
            "{amount}"
        );
        let mut function = definition("bounded", false);
        function.charge = formula;
        crate::validate_definition(&function, &BTreeMap::new()).unwrap();
        assert_eq!(
            parse_definition(&print_definition(&function)).unwrap(),
            function
        );
        assert_eq!(
            FunctionDefinition::from_json(&function.to_json().unwrap()).unwrap(),
            function
        );
        let mut registry = FunctionRegistry::new();
        let image: &'static [u8] = Vec::leak(
            registry
                .validate_functions(vec![function.clone()])
                .unwrap()
                .to_image(&|_| None),
        );
        assert_eq!(
            ReleasedFunctions::read(image)
                .unwrap()
                .definition(0)
                .unwrap(),
            function
        );
    }
}

/// K-FORM-001 / FIG-6471: do evaluates any expression for effect.
#[test]
fn do_expressions_round_trip_through_text_json_and_graph() {
    let document = crate::parse_document(
        r#"kernel 1
numbers float
effect show(value: Any) -> Any
main {
  do perform show({text: "x"}) as Any
  do {text: "discarded"}
  do join ([spawn call child()])
  do cancel (spawn call child())
  do yield
  finish null
}
fn child() { return null }
"#,
    )
    .expect("do accepts computed action arguments and pure expressions");
    crate::validate_document(&document, &crate::FunctionRegistry::new())
        .expect("valid do expressions");
    assert_eq!(
        crate::parse_document(&crate::print_document(&document)).unwrap(),
        document
    );
    assert_eq!(
        serde_json::from_str::<crate::Document>(&serde_json::to_string(&document).unwrap())
            .unwrap(),
        document
    );
    let bytes = crate::graph_tokens::encode_body(&document);
    assert_eq!(
        crate::graph_tokens::decode_body::<crate::Document>(&bytes).unwrap(),
        document
    );
}

/// K-DOC-002, K-ITER-005 (FIG-6865): what code reads of the library is
/// every function it holds, so a crossing function whose only use of a
/// helper is a loop protocol hook or a nested closure's start adapter lists
/// that helper.
#[test]
fn code_reads_list_protocol_hooks_and_start_adapters() {
    let [open, next, item, check, await_fn] = ['a', 'b', 'c', 'd', 'e']
        .map(|digit| FunctionId::parse(&digit.to_string().repeat(64)).unwrap());
    let document = parse_document(&format!(
        "kernel 1\nnumbers float\nmain {{\n\
         for x in [1] via (open @{open}, next @{next}, item @{item}, check @{check}) {{}}\n\
         let f = fn(a) start (a: Any) -> Any bare ts_direct(@{await_fn}) {{ return a }}\n\
         }}\n"
    ))
    .unwrap();
    assert_eq!(
        crate::snapshot::CodeReads::of(&[], &document.main).library,
        std::collections::BTreeSet::from([open, next, item, check, await_fn])
    );
}

/// K-DOC-002 (FIG-6865): the slot inventory is every place a document holds
/// a library function. Reading it finds each identity a document with one
/// in every kind of slot holds, and rewriting the slots it lists leaves no
/// listed identity anywhere in a document, the corpus's included.
#[test]
fn the_slot_inventory_holds_every_library_reference() {
    let ids: Vec<FunctionId> = (1..=17u8)
        .map(|n| FunctionId::parse(&format!("{n:064x}")).unwrap())
        .collect();
    let uses: String = ids
        .iter()
        .enumerate()
        .map(|(at, id)| format!("use h.f{at} = @{id}\n"))
        .collect();
    let every_slot = format!(
        "kernel 1\nnumbers float\n{uses}\
         entry run(x: Any) -> Any ts_args(h.f0, h.f1)\n\
         entry other(x: Any) -> Any ts_direct(h.f2)\n\
         fn other(x) {{ return x }}\n\
         fn run(x) start (x: Any) -> Any bare py_direct(h.f3) {{\n\
         let awaited = await x via h.f4\n\
         let nested = (await x via h.f5,)\n\
         let called = invoke h.f6(x)\n\
         let spawned = spawn invoke h.f7(x)\n\
         let nested_calls = [invoke h.f8(x), spawn invoke h.f9(x)]\n\
         let direct = h.f10(x)\n\
         for item in x via (open h.f11, next h.f12, item h.f13, check h.f14) {{}}\n\
         let f = fn(a) start (a: Any) -> Any bare ts_direct(h.f15) {{ return a }}\n\
         return x via h.f16\n\
         }}\nmain {{}}\n"
    );
    let held = |document: &Document| {
        let mut held: std::collections::BTreeSet<FunctionId> = document
            .header_functions()
            .into_iter()
            .map(|(_, function)| *function)
            .collect();
        for body in std::iter::once(&document.main)
            .chain(document.functions.values().map(|function| &function.body))
        {
            held.extend(Node::Block(body).functions_within());
        }
        held
    };
    let sentinel = FunctionId::parse(&"f".repeat(64)).unwrap();
    for text in std::iter::once(every_slot).chain(every_form_corpus()) {
        let mut document = parse_document(&text).unwrap();
        let read = held(&document);
        if text.contains("h.f16") {
            assert_eq!(read, ids.iter().copied().collect());
        }
        let mut written = std::collections::BTreeSet::new();
        document
            .try_each_function_mut(|_, function| {
                written.insert(*function);
                *function = sentinel;
                Ok::<(), std::convert::Infallible>(())
            })
            .unwrap();
        assert_eq!(written, read);
        let listed = std::mem::take(&mut document.manifest.functions);
        let json = document.to_json().unwrap();
        for function in listed.keys() {
            assert!(!json.contains(&function.to_string()), "{json}");
        }
    }
}
