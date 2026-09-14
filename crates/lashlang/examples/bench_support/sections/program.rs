/// The benchmark corpus, built from the shared AST.
///
/// ADR 0096 makes TypeScript the sole authored RLM dialect, and these programs
/// are no longer authored in the retired one. They are not authored in
/// TypeScript either: what this corpus measures is the IR and the VM, which the
/// ADR keeps, and lowering it through the dialect would change the measurement
/// rather than its spelling. TypeScript has no O(1) list append (FIG-3063) —
/// `.push()`, `.concat()` and index assignment all clone the backing vector,
/// where the IR's `push` compiles to an in-place `PushAssign` — so every
/// accumulating loop here would become quadratic: `heap_list_iteration` alone
/// measured 214x its allocation budget, and 27 of the 29 scenarios exceeded
/// theirs. The per-step-cost laws the heap scenarios exist to pin would stop
/// holding. Building the corpus with the AST constructors keeps every budget
/// exactly as calibrated and removes the dialect from a measurement that was
/// never about it.
pub fn benchmark_program(scenario: Scenario) -> Program {
    let mut declarations = benchmark_declarations();
    declarations.extend(scenario_declarations(scenario));
    b::module(declarations, benchmark_main(scenario))
}

/// The three processes every scenario can start.
fn benchmark_declarations() -> Vec<Declaration> {
    vec![
        b::process(
            "echo",
            vec![b::param("value", TypeExpr::Any)],
            b::block(vec![b::finish(b::var("value"))]),
        ),
        b::process(
            "spawn_child",
            vec![
                b::param("task", TypeExpr::Str),
                b::param("capability", TypeExpr::Str),
            ],
            b::block(vec![b::finish(b::record(vec![(
                "claim",
                b::builtin("format", vec![b::string("done:{0}"), b::var("task")]),
            )]))]),
        ),
        b::process(
            "query_llm",
            vec![
                b::param("prompt", TypeExpr::Str),
                b::param("model", TypeExpr::Str),
            ],
            b::block(vec![b::finish(b::record(vec![
                ("text", b::string("benchmark summary")),
                ("tokens", b::num(42.0)),
            ]))]),
        ),
    ]
}

/// Declarations a single scenario adds on top of the shared three.
fn scenario_declarations(scenario: Scenario) -> Vec<Declaration> {
    match scenario {
        Scenario::TriggerRegistryHostEnvironment => vec![
            b::process(
                "daily_digest",
                vec![b::param("tick", TypeExpr::Ref("cron.Tick".into()))],
                b::block(vec![b::finish(b::record(vec![
                    ("kind", b::string("daily_digest")),
                    ("fired_at", b::field(b::var("tick"), "fired_at")),
                ]))]),
            ),
            b::process(
                "on_button",
                vec![b::param("event", TypeExpr::Ref("ui.button.Pressed".into()))],
                b::block(vec![b::finish(b::record(vec![
                    ("kind", b::string("button")),
                    ("button", b::field(b::var("event"), "button")),
                ]))]),
            ),
        ],
        _ => Vec::new(),
    }
}

/// The top-level expressions of each scenario, in execution order.
pub fn benchmark_main(scenario: Scenario) -> Vec<Expr> {
    match scenario {
Scenario::Baseline => vec![
            b::assign("items", b::list(vec![b::record(vec![("label", b::string("alpha")), ("weight", b::num(1.0)), ("active", b::bool_lit(true))]), b::record(vec![("label", b::string("beta")), ("weight", b::num(2.0)), ("active", b::bool_lit(false))]), b::record(vec![("label", b::string("gamma")), ("weight", b::num(3.0)), ("active", b::bool_lit(true))])])),
            b::assign("indexes", b::builtin("range", vec![b::num(0.0), b::builtin("len", vec![b::var("items")])])),
            b::assign("all_indexes", b::builtin("push", vec![b::var("indexes"), b::builtin("len", vec![b::var("items")])])),
            b::assign("total", b::num(0.0)),
            b::assign("labels", b::list(vec![])),
            b::for_in("item", b::var("items"), b::block(vec![b::assign("total", b::binary(b::var("total"), BinaryOp::Add, b::field(b::var("item"), "weight"))), b::if_else(b::field(b::var("item"), "active"), b::block(vec![b::assign("labels", b::binary(b::var("labels"), BinaryOp::Add, b::list(vec![b::builtin("format", vec![b::string("{0}:{1}"), b::field(b::var("item"), "label"), b::field(b::var("item"), "weight")])])))]), b::block(vec![]))])),
            b::assign("lookup_handle", b::start("echo", vec![("value", b::builtin("join", vec![b::var("labels"), b::string(",")]))])),
            b::assign("stats_handle", b::start("echo", vec![("value", b::record(vec![("total", b::var("total")), ("count", b::builtin("len", vec![b::var("items")])), ("seen", b::builtin("len", vec![b::var("history")])), ("index_count", b::builtin("len", vec![b::var("all_indexes")]))]))])),
            b::assign("fanout", b::await_expr(b::list(vec![b::var("lookup_handle"), b::var("stats_handle")]))),
            b::assign("lookup_value", b::unwrap(b::index(b::var("fanout"), b::num(0.0)))),
            b::assign("stats_value", b::builtin("validate", vec![b::unwrap(b::index(b::var("fanout"), b::num(1.0))), b::type_literal(TypeExpr::Object(vec![b::type_field("total", TypeExpr::Int, false), b::type_field("count", TypeExpr::Int, false), b::type_field("seen", TypeExpr::Int, false), b::type_field("index_count", TypeExpr::Int, false)]))])),
            b::assign("summary", b::builtin("format", vec![b::string("user={0};attempt={1};active={2};total={3};count={4};seen={5};indexes={6}"), b::field(b::var("ctx"), "user"), b::field(b::var("ctx"), "attempt"), b::var("lookup_value"), b::field(b::var("stats_value"), "total"), b::field(b::var("stats_value"), "count"), b::field(b::var("stats_value"), "seen"), b::field(b::var("stats_value"), "index_count")])),
            b::finish(b::var("summary")),
        ],
        Scenario::LanguageHostEnvironment => vec![
            b::assign("source", b::builtin("join", vec![b::var("history"), b::string(",")])),
            b::assign("tokens", b::builtin("split", vec![b::var("source"), b::string(",")])),
            b::assign("trimmed_user", b::builtin("trim", vec![b::builtin("format", vec![b::string(" {0} "), b::field(b::var("ctx"), "user")])])),
            b::assign("beta_index", b::builtin("find", vec![b::var("source"), b::string("beta")])),
            b::assign("line_matches", b::builtin("grep_text", vec![b::builtin("join", vec![b::var("tokens"), b::string("\n")]), b::string("a")])),
            b::assign("count", b::builtin("len", vec![b::var("tokens")])),
            b::assign("empty_tail", b::builtin("empty", vec![b::builtin("slice", vec![b::var("tokens"), b::var("count"), b::var("count")])])),
            b::assign("predicates", b::list(vec![b::builtin("contains", vec![b::var("source"), b::index(b::var("tokens"), b::num(1.0))]), b::builtin("starts_with", vec![b::var("source"), b::index(b::var("tokens"), b::num(0.0))]), b::builtin("ends_with", vec![b::var("source"), b::index(b::var("tokens"), b::num(2.0))])])),
            b::assign("numeric", b::record(vec![("neg", b::unary(UnaryOp::Negate, b::field(b::var("ctx"), "attempt"))), ("sum", b::binary(b::field(b::var("ctx"), "attempt"), BinaryOp::Add, b::var("count"))), ("diff", b::binary(b::var("count"), BinaryOp::Subtract, b::num(1.0))), ("product", b::binary(b::var("count"), BinaryOp::Multiply, b::num(2.0))), ("quotient", b::binary(b::var("count"), BinaryOp::Divide, b::num(2.0))), ("modulo", b::binary(b::var("count"), BinaryOp::Modulo, b::num(2.0))), ("parsed_int", b::builtin("to_int", vec![b::field(b::var("ctx"), "attempt")])), ("parsed_float", b::builtin("to_float", vec![b::field(b::var("ctx"), "attempt")]))])),
            b::assign("logic", b::binary(b::unary(UnaryOp::Not, b::bool_lit(false)), BinaryOp::And, b::binary(b::binary(b::var("count"), BinaryOp::Greater, b::num(2.0)), BinaryOp::Or, b::var("empty_tail")))),
            b::assign("comparisons", b::list(vec![b::binary(b::var("count"), BinaryOp::Equal, b::num(3.0)), b::binary(b::var("count"), BinaryOp::NotEqual, b::num(4.0)), b::binary(b::var("count"), BinaryOp::Less, b::num(4.0)), b::binary(b::var("count"), BinaryOp::LessEqual, b::num(3.0)), b::binary(b::var("count"), BinaryOp::Greater, b::num(2.0)), b::binary(b::var("count"), BinaryOp::GreaterEqual, b::num(3.0))])),
            b::assign("choice", b::if_else(b::var("logic"), b::string("yes"), b::string("no"))),
            b::assign("json_text", b::binary(b::binary(b::string("{\"attempt\":"), BinaryOp::Add, b::builtin("to_string", vec![b::field(b::var("ctx"), "attempt")])), BinaryOp::Add, b::string(",\"ok\":true}"))),
            b::assign("parsed", b::builtin("json_parse", vec![b::var("json_text")])),
            b::assign("positional_results", b::await_expr(b::list(vec![b::start("echo", vec![("value", b::builtin("format", vec![b::string("left:{0}"), b::index(b::var("tokens"), b::num(0.0))]))]), b::start("echo", vec![("value", b::index(b::var("tokens"), b::num(1.0)))]), b::start("echo", vec![("value", b::builtin("len", vec![b::var("tokens")]))])]))),
            b::assign("positional", b::list(vec![b::unwrap(b::index(b::var("positional_results"), b::num(0.0))), b::index(b::var("positional_results"), b::num(1.0)), b::unwrap(b::index(b::var("positional_results"), b::num(2.0)))])),
            b::assign("named_results", b::await_expr(b::list(vec![b::start("echo", vec![("value", b::var("source"))]), b::start("echo", vec![("value", b::builtin("format", vec![b::string("{0}:{1}"), b::var("trimmed_user"), b::var("count")]))])]))),
            b::assign("named", b::record(vec![("lookup", b::unwrap(b::index(b::var("named_results"), b::num(0.0)))), ("summary", b::unwrap(b::index(b::var("named_results"), b::num(1.0))))])),
            b::assign("cancelled", b::start("echo", vec![("value", b::string("cancelled"))])),
            b::cancel(b::var("cancelled")),
            b::assign("handle", b::start("echo", vec![("value", b::string("awaited"))])),
            b::assign("awaited", b::unwrap(b::await_expr(b::var("handle")))),
            b::assign("direct", b::await_expr(b::unwrap(b::receiver_call(b::var("tools"), "echo", vec![b::record(vec![("value", b::var("trimmed_user"))])])))),
            b::print(b::var("direct")),
            b::assign("state", b::record(vec![("tags", b::builtin("push", vec![b::var("tokens"), b::string("delta")])), ("counts", b::record(vec![])), ("kept", b::list(vec![])), ("predicates", b::var("predicates")), ("comparisons", b::var("comparisons")), ("numeric", b::var("numeric")), ("parsed", b::var("parsed")), ("line_matches", b::var("line_matches"))])),
            b::assign_path("state", vec![b::field_step("tags"), b::index_step(b::num(1.0))], b::string("beta")),
            b::assign("counts", b::record(vec![])),
            b::for_in("token", b::field(b::var("state"), "tags"), b::block(vec![b::assign_path("counts", vec![b::index_step(b::var("token"))], b::binary(b::index(b::var("counts"), b::var("token")), BinaryOp::Add, b::num(1.0)))])),
            b::assign_path("state", vec![b::field_step("counts")], b::var("counts")),
            b::for_in("token", b::builtin("keys", vec![b::field(b::var("state"), "counts")]), b::block(vec![b::if_else(b::binary(b::var("token"), BinaryOp::Equal, b::string("beta")), b::block(vec![Expr::Continue]), b::block(vec![])), b::if_else(b::binary(b::var("token"), BinaryOp::Equal, b::string("delta")), b::block(vec![Expr::Break]), b::block(vec![])), b::assign_path("state", vec![b::field_step("kept")], b::builtin("push", vec![b::field(b::var("state"), "kept"), b::var("token")]))])),
            b::assign("Payload", b::type_literal(TypeExpr::Object(vec![b::type_field("user", TypeExpr::Str, false), b::type_field("choice", TypeExpr::Enum(vec!["yes".into(), "no".into()]), false), b::type_field("tags", TypeExpr::List(Box::new(TypeExpr::Str)), false), b::type_field("counts", TypeExpr::Dict, false), b::type_field("kept", TypeExpr::List(Box::new(TypeExpr::Str)), false), b::type_field("maybe", TypeExpr::Union(vec![TypeExpr::Str, TypeExpr::Null]), false), b::type_field("optional_note", TypeExpr::Str, true)]))),
            b::assign("validated", b::builtin("validate", vec![b::record(vec![("user", b::var("direct")), ("choice", b::var("choice")), ("tags", b::field(b::var("state"), "tags")), ("counts", b::field(b::var("state"), "counts")), ("kept", b::field(b::var("state"), "kept")), ("maybe", b::null())]), b::var("Payload")])),
            b::finish(b::record(vec![("direct", b::var("direct")), ("awaited", b::var("awaited")), ("positional", b::var("positional")), ("lookup", b::field(b::var("named"), "lookup")), ("summary", b::field(b::var("named"), "summary")), ("values", b::builtin("values", vec![b::field(b::var("state"), "counts")])), ("beta_index", b::var("beta_index")), ("line_matches", b::var("line_matches")), ("first_two", b::builtin("slice", vec![b::field(b::var("state"), "tags"), b::null(), b::num(2.0)])), ("validated", b::var("validated")), ("stringified", b::builtin("to_string", vec![b::var("validated")]))])),
        ],
        Scenario::AsyncAwait => vec![
            b::assign("handles", b::list(vec![b::start("echo", vec![("value", b::string("alpha"))]), b::start("echo", vec![("value", b::string("beta"))]), b::start("echo", vec![("value", b::string("gamma"))])])),
            b::assign("results", b::await_expr(b::var("handles"))),
            b::assign("formatted", b::list(vec![b::unwrap(b::index(b::var("results"), b::num(0.0))), b::unwrap(b::index(b::var("results"), b::num(1.0))), b::unwrap(b::index(b::var("results"), b::num(2.0)))])),
            b::finish(b::builtin("join", vec![b::var("formatted"), b::string(",")])),
        ],
        Scenario::DirectUnwrap => vec![
            b::assign("first", b::await_expr(b::unwrap(b::receiver_call(b::var("tools"), "echo", vec![b::record(vec![("value", b::string("alpha"))])])))),
            b::assign("second", b::await_expr(b::unwrap(b::receiver_call(b::var("tools"), "echo", vec![b::record(vec![("value", b::builtin("format", vec![b::string("{0}:{1}"), b::var("first"), b::string("beta")]))])])))),
            b::assign("third", b::await_expr(b::unwrap(b::receiver_call(b::var("tools"), "echo", vec![b::record(vec![("value", b::builtin("join", vec![b::list(vec![b::var("first"), b::var("second")]), b::string(",")]))])])))),
            b::finish(b::var("third")),
        ],
        Scenario::GeneralFanout => vec![
            b::assign("seed", b::list(vec![b::string("alpha"), b::string("beta"), b::string("gamma")])),
            b::assign("results", b::await_expr(b::list(vec![b::start("echo", vec![("value", b::builtin("format", vec![b::string("{0}:{1}"), b::index(b::var("seed"), b::num(0.0)), b::builtin("len", vec![b::var("seed")])]))]), b::start("echo", vec![("value", b::builtin("format", vec![b::string("{0}:{1}"), b::index(b::var("seed"), b::num(1.0)), b::builtin("len", vec![b::var("seed")])]))])]))),
            b::finish(b::builtin("format", vec![b::string("{0}|{1}"), b::unwrap(b::index(b::var("results"), b::num(0.0))), b::unwrap(b::index(b::var("results"), b::num(1.0)))])),
        ],
        Scenario::LoopControl => vec![
            b::assign("items", b::builtin("range", vec![b::num(0.0), b::num(128.0)])),
            b::assign("outer", b::string("restored")),
            b::assign("kept", b::num(0.0)),
            b::assign("skipped", b::num(0.0)),
            b::for_in("outer", b::var("items"), b::block(vec![b::if_else(b::binary(b::var("outer"), BinaryOp::Less, b::num(32.0)), b::block(vec![b::assign("skipped", b::binary(b::var("skipped"), BinaryOp::Add, b::num(1.0))), Expr::Continue]), b::block(vec![])), b::if_else(b::binary(b::var("outer"), BinaryOp::GreaterEqual, b::num(96.0)), b::block(vec![Expr::Break]), b::block(vec![])), b::if_else(b::binary(b::binary(b::var("outer"), BinaryOp::Modulo, b::num(3.0)), BinaryOp::Equal, b::num(0.0)), b::block(vec![Expr::Continue]), b::block(vec![])), b::assign("kept", b::binary(b::var("kept"), BinaryOp::Add, b::num(1.0)))])),
            b::finish(b::record(vec![("kept", b::var("kept")), ("skipped", b::var("skipped")), ("outer", b::var("outer"))])),
        ],
        Scenario::IndexedAssignment => vec![
            b::assign("groups", b::list(vec![b::string("alpha"), b::string("beta"), b::string("alpha"), b::string("gamma"), b::string("beta"), b::string("alpha"), b::string("delta"), b::string("gamma")])),
            b::assign("counts", b::record(vec![])),
            b::for_in("group", b::var("groups"), b::block(vec![b::assign_path("counts", vec![b::index_step(b::var("group"))], b::binary(b::index(b::var("counts"), b::var("group")), BinaryOp::Add, b::num(1.0)))])),
            b::assign("state", b::record(vec![("groups", b::record(vec![("alpha", b::record(vec![("count", b::num(0.0))])), ("beta", b::record(vec![("count", b::num(0.0))])), ("gamma", b::record(vec![("count", b::num(0.0))])), ("delta", b::record(vec![("count", b::num(0.0))]))]))])),
            b::for_in("group", b::builtin("keys", vec![b::var("counts")]), b::block(vec![b::assign_path("state", vec![b::field_step("groups"), b::index_step(b::var("group")), b::field_step("count")], b::index(b::var("counts"), b::var("group")))])),
            b::assign("summary", b::list(vec![])),
            b::for_in("group", b::builtin("keys", vec![b::var("counts")]), b::block(vec![b::assign("summary", b::binary(b::var("summary"), BinaryOp::Add, b::list(vec![b::builtin("format", vec![b::string("{0}:{1}"), b::var("group"), b::field(b::index(b::field(b::var("state"), "groups"), b::var("group")), "count")])])))])),
            b::finish(b::record(vec![("counts", b::var("counts")), ("state", b::var("state")), ("summary", b::builtin("join", vec![b::var("summary"), b::string(",")]))])),
        ],
        Scenario::ProjectedValues => vec![
            b::assign("first", b::index(b::var("history"), b::num(0.0))),
            b::assign("second", b::index(b::var("history"), b::num(1.0))),
            b::assign("body_head", b::index(b::field(b::var("docs"), "body"), b::num(0.0))),
            b::assign("body_match_index", b::builtin("find", vec![b::field(b::var("docs"), "body"), b::string("markdown")])),
            b::assign("body_matches", b::builtin("grep_text", vec![b::field(b::var("docs"), "body"), b::string("markdown")])),
            b::assign("second_matches", b::builtin("grep_text", vec![b::field(b::var("second"), "content"), b::string("response")])),
            b::if_else(b::field(b::var("docs"), "body"), b::block(vec![b::assign("body_truthy", b::bool_lit(true))]), b::block(vec![b::assign("body_truthy", b::bool_lit(false))])),
            b::print(b::field(b::var("docs"), "body")),
            b::assign("summary", b::record(vec![("history_len", b::builtin("len", vec![b::var("history")])), ("first_role", b::field(b::var("first"), "role")), ("first_content", b::field(b::var("first"), "content")), ("second_content", b::field(b::var("second"), "content")), ("doc_title", b::field(b::var("docs"), "title")), ("doc_summary", b::field(b::var("docs"), "summary")), ("body_head", b::var("body_head")), ("body_match_index", b::var("body_match_index")), ("body_matches", b::var("body_matches")), ("second_matches", b::var("second_matches")), ("body_truthy", b::var("body_truthy")), ("body_text", b::field(b::var("docs"), "body"))])),
            b::finish(b::var("summary")),
        ],
        Scenario::LargeData => vec![
            b::assign("items", b::builtin("range", vec![b::num(0.0), b::num(512.0)])),
            b::assign("groups", b::record(vec![])),
            b::assign("total", b::num(0.0)),
            b::assign("evens", b::list(vec![])),
            b::assign("odds", b::list(vec![])),
            b::for_in("item", b::var("items"), b::block(vec![b::assign("key", b::builtin("format", vec![b::string("bucket_{0}"), b::binary(b::var("item"), BinaryOp::Modulo, b::num(16.0))])), b::assign_path("groups", vec![b::index_step(b::var("key"))], b::binary(b::index(b::var("groups"), b::var("key")), BinaryOp::Add, b::num(1.0))), b::assign("total", b::binary(b::var("total"), BinaryOp::Add, b::var("item"))), b::if_else(b::binary(b::binary(b::var("item"), BinaryOp::Modulo, b::num(2.0)), BinaryOp::Equal, b::num(0.0)), b::block(vec![b::assign("evens", b::builtin("push", vec![b::var("evens"), b::var("item")]))]), b::block(vec![b::assign("odds", b::builtin("push", vec![b::var("odds"), b::var("item")]))]))])),
            b::assign("lines", b::list(vec![])),
            b::for_in("key", b::builtin("keys", vec![b::var("groups")]), b::block(vec![b::assign("lines", b::builtin("push", vec![b::var("lines"), b::builtin("format", vec![b::string("{0}:{1}"), b::var("key"), b::index(b::var("groups"), b::var("key"))])]))])),
            b::assign("payload", b::record(vec![("count", b::builtin("len", vec![b::var("items")])), ("total", b::var("total")), ("groups", b::var("groups")), ("evens", b::builtin("len", vec![b::var("evens")])), ("odds", b::builtin("len", vec![b::var("odds")])), ("summary", b::builtin("join", vec![b::var("lines"), b::string("|")]))])),
            b::finish(b::builtin("validate", vec![b::var("payload"), b::type_literal(TypeExpr::Object(vec![b::type_field("count", TypeExpr::Int, false), b::type_field("total", TypeExpr::Int, false), b::type_field("groups", TypeExpr::Dict, false), b::type_field("evens", TypeExpr::Int, false), b::type_field("odds", TypeExpr::Int, false), b::type_field("summary", TypeExpr::Str, false)]))])),
        ],
        Scenario::CachePressure => vec![
            b::assign("seed", b::record(vec![("user", b::field(b::var("ctx"), "user")), ("attempt", b::field(b::var("ctx"), "attempt")), ("history_len", b::builtin("len", vec![b::var("history")])), ("labels", b::list(vec![b::string("alpha"), b::string("beta"), b::string("gamma"), b::string("delta"), b::string("epsilon"), b::string("zeta")]))])),
            b::assign("a0", b::builtin("format", vec![b::string("{0}:{1}"), b::index(b::field(b::var("seed"), "labels"), b::num(0.0)), b::field(b::var("seed"), "attempt")])),
            b::assign("a1", b::builtin("format", vec![b::string("{0}:{1}"), b::index(b::field(b::var("seed"), "labels"), b::num(1.0)), b::field(b::var("seed"), "history_len")])),
            b::assign("a2", b::builtin("format", vec![b::string("{0}:{1}"), b::index(b::field(b::var("seed"), "labels"), b::num(2.0)), b::builtin("len", vec![b::var("a0")])])),
            b::assign("a3", b::builtin("format", vec![b::string("{0}:{1}"), b::index(b::field(b::var("seed"), "labels"), b::num(3.0)), b::builtin("len", vec![b::var("a1")])])),
            b::assign("a4", b::builtin("format", vec![b::string("{0}:{1}"), b::index(b::field(b::var("seed"), "labels"), b::num(4.0)), b::builtin("len", vec![b::var("a2")])])),
            b::assign("a5", b::builtin("format", vec![b::string("{0}:{1}"), b::index(b::field(b::var("seed"), "labels"), b::num(5.0)), b::builtin("len", vec![b::var("a3")])])),
            b::assign("r0", b::record(vec![("name", b::string("r0")), ("value", b::var("a0")), ("next", b::var("a1"))])),
            b::assign("r1", b::record(vec![("name", b::string("r1")), ("value", b::var("a1")), ("next", b::var("a2"))])),
            b::assign("r2", b::record(vec![("name", b::string("r2")), ("value", b::var("a2")), ("next", b::var("a3"))])),
            b::assign("r3", b::record(vec![("name", b::string("r3")), ("value", b::var("a3")), ("next", b::var("a4"))])),
            b::assign("r4", b::record(vec![("name", b::string("r4")), ("value", b::var("a4")), ("next", b::var("a5"))])),
            b::assign("r5", b::record(vec![("name", b::string("r5")), ("value", b::var("a5")), ("next", b::var("a0"))])),
            b::assign("TypeA", b::type_literal(TypeExpr::Object(vec![b::type_field("name", TypeExpr::Str, false), b::type_field("value", TypeExpr::Str, false), b::type_field("next", TypeExpr::Str, false)]))),
            b::assign("validated", b::list(vec![b::builtin("validate", vec![b::var("r0"), b::var("TypeA")]), b::builtin("validate", vec![b::var("r1"), b::var("TypeA")]), b::builtin("validate", vec![b::var("r2"), b::var("TypeA")]), b::builtin("validate", vec![b::var("r3"), b::var("TypeA")]), b::builtin("validate", vec![b::var("r4"), b::var("TypeA")]), b::builtin("validate", vec![b::var("r5"), b::var("TypeA")])])),
            b::finish(b::builtin("join", vec![b::list(vec![b::field(b::index(b::var("validated"), b::num(0.0)), "value"), b::field(b::index(b::var("validated"), b::num(1.0)), "value"), b::field(b::index(b::var("validated"), b::num(2.0)), "value"), b::field(b::index(b::var("validated"), b::num(3.0)), "value"), b::field(b::index(b::var("validated"), b::num(4.0)), "value"), b::field(b::index(b::var("validated"), b::num(5.0)), "value")]), b::string("|")])),
        ],
        Scenario::ProjectedOperations => vec![
            b::assign("summary", b::record(vec![("len", b::builtin("len", vec![b::field(b::var("proj"), "items")])), ("empty", b::builtin("empty", vec![b::field(b::var("proj"), "items")])), ("keys", b::builtin("keys", vec![b::field(b::var("proj"), "record")])), ("values", b::builtin("values", vec![b::field(b::var("proj"), "record")])), ("contains", b::builtin("contains", vec![b::field(b::var("proj"), "items"), b::string("beta")])), ("starts", b::builtin("starts_with", vec![b::field(b::var("proj"), "text"), b::string("alpha")])), ("ends", b::builtin("ends_with", vec![b::field(b::var("proj"), "text"), b::string("delta")])), ("split_count", b::builtin("len", vec![b::builtin("split", vec![b::field(b::var("proj"), "text"), b::string(" ")])])), ("join", b::builtin("join", vec![b::field(b::var("proj"), "items"), b::string(",")])), ("trim", b::builtin("trim", vec![b::field(b::var("proj"), "padded")])), ("slice_text", b::builtin("slice", vec![b::field(b::var("proj"), "text"), b::num(6.0), b::num(10.0)])), ("slice_list", b::builtin("slice", vec![b::field(b::var("proj"), "items"), b::num(1.0), b::num(3.0)])), ("pushed", b::builtin("push", vec![b::field(b::var("proj"), "items"), b::string("epsilon")])), ("as_int", b::builtin("to_int", vec![b::field(b::var("proj"), "number")])), ("as_float", b::builtin("to_float", vec![b::field(b::var("proj"), "number")])), ("parsed", b::builtin("json_parse", vec![b::field(b::var("proj"), "json")])), ("first", b::index(b::field(b::var("proj"), "items"), b::num(0.0))), ("field", b::field(b::field(b::var("proj"), "record"), "topic"))])),
            b::finish(b::var("summary")),
        ],
        Scenario::TypeSystemStress => vec![
            b::assign("Meta", b::type_literal(TypeExpr::Object(vec![b::type_field("source", TypeExpr::Str, false), b::type_field("attempt", TypeExpr::Int, false), b::type_field("tags", TypeExpr::List(Box::new(TypeExpr::Enum(vec!["alpha".into(), "beta".into(), "gamma".into(), "delta".into()]))), false), b::type_field("optional_note", TypeExpr::Str, true)]))),
            b::assign("Item", b::type_literal(TypeExpr::Object(vec![b::type_field("id", TypeExpr::Int, false), b::type_field("title", TypeExpr::Str, false), b::type_field("score", TypeExpr::Float, false), b::type_field("active", TypeExpr::Bool, false), b::type_field("meta", TypeExpr::Ref("Meta".into()), false), b::type_field("maybe", TypeExpr::Union(vec![TypeExpr::Str, TypeExpr::Int, TypeExpr::Null]), false)]))),
            b::assign("items", b::list(vec![])),
            b::for_in("i", b::builtin("range", vec![b::num(0.0), b::num(64.0)]), b::block(vec![b::assign("raw", b::record(vec![("id", b::var("i")), ("title", b::builtin("format", vec![b::string("item-{0}"), b::var("i")])), ("score", b::binary(b::var("i"), BinaryOp::Divide, b::num(2.0))), ("active", b::binary(b::binary(b::var("i"), BinaryOp::Modulo, b::num(2.0)), BinaryOp::Equal, b::num(0.0))), ("meta", b::record(vec![("source", b::field(b::var("ctx"), "user")), ("attempt", b::field(b::var("ctx"), "attempt")), ("tags", b::list(vec![b::string("alpha"), b::string("beta"), b::string("gamma")]))])), ("maybe", b::if_else(b::binary(b::binary(b::var("i"), BinaryOp::Modulo, b::num(3.0)), BinaryOp::Equal, b::num(0.0)), b::null(), b::builtin("format", vec![b::string("v{0}"), b::var("i")])))])), b::assign("items", b::builtin("push", vec![b::var("items"), b::builtin("validate", vec![b::var("raw"), b::var("Item")])]))])),
            b::finish(b::record(vec![("count", b::builtin("len", vec![b::var("items")])), ("first", b::field(b::index(b::var("items"), b::num(0.0)), "title")), ("last", b::field(b::index(b::var("items"), b::num(63.0)), "title")), ("tags", b::builtin("join", vec![b::field(b::field(b::index(b::var("items"), b::num(1.0)), "meta"), "tags"), b::string(",")]))])),
        ],
        Scenario::WrappedErrorPaths => vec![
            b::assign("missing", b::await_expr(b::receiver_call(b::var("tools"), "missing_tool", vec![b::record(vec![("value", b::string("x"))])]))),
            b::assign("boom", b::await_expr(b::receiver_call(b::var("tools"), "boom", vec![b::record(vec![("reason", b::string("explicit"))])]))),
            b::assign("ok", b::await_expr(b::receiver_call(b::var("tools"), "echo", vec![b::record(vec![("value", b::string("still-running"))])]))),
            b::assign("probe", b::await_expr(b::receiver_call(b::var("shell"), "exec", vec![b::record(vec![("cmd", b::string("test -f Cargo.lock"))])]))),
            b::finish(b::record(vec![("missing_ok", b::field(b::var("missing"), "ok")), ("missing_error", b::builtin("contains", vec![b::field(b::var("missing"), "error"), b::string("unknown tool")])), ("boom_ok", b::field(b::var("boom"), "ok")), ("boom_error", b::builtin("contains", vec![b::field(b::var("boom"), "error"), b::string("explicit failure")])), ("ok_value", b::field(b::var("ok"), "value")), ("probe_exit", b::field(b::field(b::var("probe"), "value"), "exit_code")), ("probe_done", b::field(b::field(b::var("probe"), "value"), "done"))])),
        ],
        Scenario::ToolControlHostEnvironment => vec![
            b::assign("first", b::start("spawn_child", vec![("task", b::string("inspect auth")), ("capability", b::string("explore"))])),
            b::assign("second", b::start("spawn_child", vec![("task", b::string("inspect api")), ("capability", b::string("explore"))])),
            b::assign("llm", b::start("query_llm", vec![("prompt", b::string("summarize benchmark")), ("model", b::string("gpt-5.4-mini"))])),
            b::assign("probe", b::start("echo", vec![("value", b::string("app log"))])),
            b::assign("handles", b::await_expr(b::unwrap(b::receiver_call(b::var("processes"), "list", vec![b::record(vec![])])))),
            b::assign("results", b::await_expr(b::list(vec![b::var("first"), b::var("second"), b::var("llm"), b::var("probe")]))),
            b::cancel(b::var("second")),
            b::finish(b::record(vec![("first", b::field(b::field(b::index(b::var("results"), b::num(0.0)), "value"), "claim")), ("second", b::field(b::field(b::index(b::var("results"), b::num(1.0)), "value"), "claim")), ("llm", b::field(b::field(b::index(b::var("results"), b::num(2.0)), "value"), "text")), ("probe", b::field(b::index(b::var("results"), b::num(3.0)), "value")), ("tools", b::builtin("len", vec![b::var("handles")]))])),
        ],
        Scenario::SnapshotProjectedState => vec![
            b::assign("head", b::builtin("slice", vec![b::field(b::field(b::var("snap"), "projected"), "body"), b::num(0.0), b::num(16.0)])),
            b::assign("materialized", b::builtin("to_string", vec![b::field(b::field(b::var("snap"), "projected"), "body")])),
            b::assign("nested_head", b::field(b::field(b::field(b::var("snap"), "mixed"), "nested"), "projected_title")),
            b::assign_path("snap", vec![b::field_step("mixed"), b::field_step("count")], b::binary(b::field(b::field(b::var("snap"), "mixed"), "count"), BinaryOp::Add, b::num(1.0))),
            b::finish(b::record(vec![("id", b::field(b::var("snap"), "id")), ("normal_title", b::field(b::field(b::var("snap"), "normal"), "title")), ("head", b::var("head")), ("materialized_len", b::builtin("len", vec![b::var("materialized")])), ("nested_head", b::var("nested_head")), ("count", b::field(b::field(b::var("snap"), "mixed"), "count")), ("tags", b::builtin("join", vec![b::field(b::field(b::var("snap"), "normal"), "tags"), b::string(",")]))])),
        ],
        Scenario::ContinueAsSeedHostEnvironment => vec![
            b::assign("agent", b::start("spawn_child", vec![("task", b::string("inspect carry-forward")), ("capability", b::string("explore"))])),
            b::assign("handles", b::await_expr(b::unwrap(b::receiver_call(b::var("processes"), "list", vec![b::record(vec![])])))),
            b::assign("frame", b::await_expr(b::unwrap(b::receiver_call(b::var("control"), "continue_as", vec![b::record(vec![("task", b::string("continue from compact state")), ("seed", b::record(vec![("projected_problem", b::field(b::var("proj"), "text")), ("nested_projected", b::record(vec![("body", b::field(b::var("proj"), "json"))])), ("computed_summary", b::builtin("format", vec![b::string("{0}:{1}"), b::field(b::var("ctx"), "user"), b::builtin("len", vec![b::var("history")])])), ("live_agent", b::index(b::var("handles"), b::num(0.0))), ("started_agent", b::var("agent"))]))])])))),
            b::finish(b::record(vec![("frame_key", b::field(b::var("frame"), "frame_key")), ("task", b::field(b::var("frame"), "task")), ("seed_keys", b::field(b::var("frame"), "seed_keys")), ("projected_count", b::field(b::var("frame"), "projected_count")), ("global_count", b::field(b::var("frame"), "global_count"))])),
        ],
        Scenario::TriggerRegistryHostEnvironment => vec![
            b::assign("daily_handle", b::await_expr(b::unwrap(b::receiver_call(b::var("triggers"), "register", vec![b::record(vec![("source", b::receiver_call(b::var("cron"), "Schedule", vec![b::record(vec![("expr", b::string("0 8 * * *")), ("tz", b::string("UTC"))])])), ("target", b::var("daily_digest")), ("inputs", b::record(vec![("tick", b::field(b::var("trigger"), "event"))])), ("name", b::string("daily_digest")), ("subscription_key", b::string("daily-digest"))])])))),
            b::assign("button_handle", b::await_expr(b::unwrap(b::receiver_call(b::var("triggers"), "register", vec![b::record(vec![("source", b::receiver_call(b::field(b::var("ui"), "button"), "pressed", vec![b::record(vec![])])), ("target", b::var("on_button")), ("inputs", b::record(vec![("event", b::field(b::var("trigger"), "event"))])), ("name", b::string("button watcher")), ("subscription_key", b::string("button-watcher"))])])))),
            b::assign("registrations", b::await_expr(b::unwrap(b::receiver_call(b::var("triggers"), "list", vec![b::record(vec![("target", b::var("daily_digest"))])])))),
            b::assign("disabled", b::await_expr(b::unwrap(b::receiver_call(b::var("triggers"), "disable", vec![b::record(vec![("subscription_key", b::string("daily-digest")), ("expected_revision", b::field(b::var("daily_handle"), "revision"))])])))),
            b::finish(b::record(vec![("daily_handle", b::field(b::var("daily_handle"), "id")), ("button_handle", b::field(b::var("button_handle"), "id")), ("registration_count", b::builtin("len", vec![b::var("registrations")])), ("listed_target", b::field(b::field(b::index(b::var("registrations"), b::num(0.0)), "target"), "process_name")), ("listed_source", b::field(b::index(b::var("registrations"), b::num(0.0)), "source_type")), ("disabled", b::field(b::var("disabled"), "enabled"))])),
        ],
        Scenario::SyntaxTextHostEnvironment => vec![
            b::assign("patch", b::string("*** Begin Patch\n*** Update File: crates/lashlang/src/lib.rs\n@@\n-old\n+new\n\\n { braces stay raw }\n*** End Patch")),
            b::assign("script", b::string("python3 - <<'PY'\nprint(\"\"\"double quotes are preserved\"\"\")\n\\n { braces stay raw }\nPY")),
            b::assign("plain", b::string("first\n\"quoted\"\nsecond")),
            b::string("bare expression branch"),
            b::assign("pieces", b::list(vec![b::builtin("len", vec![b::var("patch")]), b::builtin("contains", vec![b::var("patch"), b::string("*** Begin Patch")]), b::builtin("starts_with", vec![b::var("script"), b::string("python3")]), b::builtin("ends_with", vec![b::builtin("trim", vec![b::var("script")]), b::string("PY")]), b::builtin("len", vec![b::builtin("split", vec![b::var("plain"), b::string("\n")])]), b::builtin("slice", vec![b::var("plain"), b::num(0.0), b::num(5.0)])])),
            b::finish(b::record(vec![("patch_head", b::builtin("slice", vec![b::var("patch"), b::num(0.0), b::num(15.0)])), ("script_head", b::builtin("slice", vec![b::var("script"), b::num(0.0), b::num(7.0)])), ("plain_lines", b::builtin("len", vec![b::builtin("split", vec![b::var("plain"), b::string("\n")])])), ("pieces", b::var("pieces"))])),
        ],
        Scenario::IntegerRangeHostEnvironment => vec![
            b::assign("items", b::builtin("range", vec![b::unary(UnaryOp::Negate, b::num(8.0)), b::num(9.0)])),
            b::assign("forward", b::builtin("range", vec![b::num(0.0), b::num(10.0), b::num(3.0)])),
            b::assign("backward", b::builtin("range", vec![b::num(7.0), b::unary(UnaryOp::Negate, b::num(3.0)), b::unary(UnaryOp::Negate, b::num(2.0))])),
            b::assign("stride", b::builtin("ceil_div", vec![b::builtin("len", vec![b::var("items")]), b::num(4.0)])),
            b::assign("starts", b::list(vec![])),
            b::assign("windows", b::list(vec![])),
            b::for_in("i", b::builtin("range", vec![b::num(0.0), b::builtin("len", vec![b::var("items")]), b::var("stride")]), b::block(vec![b::assign("starts", b::builtin("push", vec![b::var("starts"), b::var("i")])), b::assign("windows", b::builtin("push", vec![b::var("windows"), b::builtin("slice", vec![b::var("items"), b::var("i"), b::binary(b::var("i"), BinaryOp::Add, b::var("stride"))])]))])),
            b::assign("text", b::string("alpha beta gamma beta delta")),
            b::assign("first_beta", b::builtin("find", vec![b::var("text"), b::string("beta")])),
            b::assign("second_beta", b::builtin("find", vec![b::var("text"), b::string("beta"), b::binary(b::var("first_beta"), BinaryOp::Add, b::num(1.0))])),
            b::finish(b::record(vec![("count", b::builtin("len", vec![b::var("items")])), ("first", b::index(b::var("items"), b::num(0.0))), ("last", b::index(b::var("items"), b::binary(b::builtin("len", vec![b::var("items")]), BinaryOp::Subtract, b::num(1.0)))), ("forward", b::var("forward")), ("backward", b::var("backward")), ("stride", b::var("stride")), ("starts", b::var("starts")), ("windows", b::var("windows")), ("mid", b::builtin("slice", vec![b::var("items"), b::num(2.0), b::unary(UnaryOp::Negate, b::num(2.0))])), ("head", b::builtin("slice", vec![b::var("text"), b::null(), b::num(5.0)])), ("tail", b::builtin("slice", vec![b::var("text"), b::unary(UnaryOp::Negate, b::num(5.0)), b::null()])), ("first_beta", b::var("first_beta")), ("second_beta", b::var("second_beta")), ("ceil_neg", b::builtin("ceil_div", vec![b::unary(UnaryOp::Negate, b::num(10.0)), b::num(3.0)])), ("floor_neg", b::builtin("floor_div", vec![b::unary(UnaryOp::Negate, b::num(10.0)), b::num(3.0)]))])),
        ],
        Scenario::FanoutExpressionHostEnvironment => vec![
            b::assign("left", b::await_expr(b::receiver_call(b::var("tools"), "echo", vec![b::record(vec![("value", b::string("left"))])]))),
            b::assign("right", b::await_expr(b::receiver_call(b::var("tools"), "echo", vec![b::record(vec![("value", b::string("right"))])]))),
            b::assign("computed", b::binary(b::builtin("len", vec![b::var("history")]), BinaryOp::Add, b::num(39.0))),
            b::assign("discarded", b::list(vec![b::string("branch_a"), b::binary(b::num(40.0), BinaryOp::Add, b::num(2.0)), b::builtin("len", vec![b::var("history")])])),
            b::assign("batched_results", b::await_expr(b::list(vec![b::start("echo", vec![("value", b::field(b::var("left"), "value"))]), b::start("echo", vec![("value", b::field(b::var("right"), "value"))])]))),
            b::assign("batched", b::record(vec![("first", b::unwrap(b::index(b::var("batched_results"), b::num(0.0)))), ("second", b::unwrap(b::index(b::var("batched_results"), b::num(1.0)))), ("computed", b::var("computed"))])),
            b::finish(b::record(vec![("left", b::field(b::var("left"), "value")), ("right", b::field(b::var("right"), "value")), ("computed", b::var("computed")), ("discarded", b::var("discarded")), ("first", b::field(b::var("batched"), "first")), ("second", b::field(b::var("batched"), "second")), ("batched_computed", b::field(b::var("batched"), "computed"))])),
        ],
        Scenario::ImageHostEnvironment => vec![
            b::assign("descriptor", b::builtin("to_string", vec![b::var("img")])),
            b::assign("metadata", b::record(vec![("id", b::field(b::var("img"), "id")), ("label", b::field(b::var("img"), "label")), ("size", b::field(b::var("img"), "size")), ("width", b::field(b::var("img"), "width")), ("height", b::field(b::var("img"), "height")), ("missing", b::field(b::var("img"), "missing"))])),
            b::print(b::var("img")),
            b::finish(b::record(vec![("metadata", b::var("metadata")), ("descriptor_has_type", b::builtin("contains", vec![b::var("descriptor"), b::string("\"type\":\"image\"")])), ("descriptor_has_id", b::builtin("contains", vec![b::var("descriptor"), b::string("\"id\":\"img-1\"")])), ("dims", b::builtin("format", vec![b::string("{0}x{1}"), b::field(b::var("img"), "width"), b::field(b::var("img"), "height")])), ("size_bucket", b::builtin("floor_div", vec![b::field(b::var("img"), "size"), b::num(100.0)]))])),
        ],
        Scenario::HeapListIteration => vec![
            b::assign("rows", b::list(vec![])),
            b::for_in("n", b::builtin("range", vec![b::num(0.0), b::num(2000.0)]), b::block(vec![b::assign("rows", b::builtin("push", vec![b::var("rows"), b::var("n")]))])),
            b::assign("total", b::num(0.0)),
            b::assign("seen", b::num(0.0)),
            b::for_in("row", b::var("rows"), b::block(vec![b::assign("total", b::binary(b::var("total"), BinaryOp::Add, b::var("row"))), b::assign("seen", b::binary(b::var("seen"), BinaryOp::Add, b::num(1.0)))])),
            b::finish(b::record(vec![("total", b::var("total")), ("seen", b::var("seen"))])),
        ],
        Scenario::HeapNestedLoop => vec![
            b::assign("rows", b::list(vec![])),
            b::assign("checksum", b::num(0.0)),
            b::for_in("n", b::builtin("range", vec![b::num(0.0), b::num(60.0)]), b::block(vec![b::assign("rows", b::builtin("push", vec![b::var("rows"), b::list(vec![b::var("n"), b::binary(b::var("n"), BinaryOp::Add, b::num(1.0))])])), b::for_in("row", b::var("rows"), b::block(vec![b::assign("checksum", b::binary(b::var("checksum"), BinaryOp::Add, b::index(b::var("row"), b::num(0.0))))]))])),
            b::finish(b::record(vec![("checksum", b::var("checksum")), ("rows", b::builtin("len", vec![b::var("rows")]))])),
        ],
        Scenario::HeapAllocationChurn => vec![
            b::assign("kept", b::list(vec![])),
            b::for_in("n", b::builtin("range", vec![b::num(0.0), b::num(400.0)]), b::block(vec![b::assign("scratch", b::record(vec![("index", b::var("n")), ("pair", b::list(vec![b::var("n"), b::binary(b::var("n"), BinaryOp::Add, b::num(1.0))])), ("label", b::builtin("format", vec![b::string("row-{0}"), b::var("n")]))])), b::if_else(b::binary(b::binary(b::var("n"), BinaryOp::Modulo, b::num(40.0)), BinaryOp::Equal, b::num(0.0)), b::block(vec![b::assign("kept", b::builtin("push", vec![b::var("kept"), b::var("scratch")]))]), b::block(vec![]))])),
            b::finish(b::record(vec![("kept", b::builtin("len", vec![b::var("kept")]))])),
        ],
        Scenario::HeapDeepChainMutation => vec![
            b::assign("tree", b::record(vec![("level", b::record(vec![("rows", b::list(vec![b::list(vec![b::num(0.0)]), b::list(vec![b::num(1.0)]), b::list(vec![b::num(2.0)])])), ("counters", b::record(vec![("c", b::num(0.0))]))]))])),
            b::for_in("n", b::builtin("range", vec![b::num(0.0), b::num(150.0)]), b::block(vec![b::assign_path("tree", vec![b::field_step("level"), b::field_step("rows"), b::index_step(b::binary(b::var("n"), BinaryOp::Modulo, b::num(3.0)))], b::list(vec![b::var("n")])), b::assign_path("tree", vec![b::field_step("level"), b::field_step("counters"), b::index_step(b::string("c"))], b::binary(b::index(b::field(b::field(b::var("tree"), "level"), "counters"), b::string("c")), BinaryOp::Add, b::num(1.0)))])),
            b::finish(b::record(vec![("counter", b::index(b::field(b::field(b::var("tree"), "level"), "counters"), b::string("c"))), ("first", b::index(b::field(b::field(b::var("tree"), "level"), "rows"), b::num(0.0)))])),
        ],
        Scenario::HeapComprehensionBuild => vec![
            b::assign("source", b::list(vec![])),
            b::for_in("n", b::builtin("range", vec![b::num(0.0), b::num(800.0)]), b::block(vec![b::assign("source", b::builtin("push", vec![b::var("source"), b::var("n")]))])),
            b::assign("doubled", b::comprehension(b::binary(b::var("item"), BinaryOp::Add, b::var("item")), vec![b::comprehension_for("item", b::var("source"))])),
            b::assign("tagged", b::comprehension(b::list(vec![b::var("item")]), vec![b::comprehension_for("item", b::var("source")), b::comprehension_if(b::binary(b::binary(b::var("item"), BinaryOp::Modulo, b::num(7.0)), BinaryOp::Equal, b::num(0.0)))])),
            b::finish(b::record(vec![("doubled", b::builtin("len", vec![b::var("doubled")])), ("tagged", b::builtin("len", vec![b::var("tagged")])), ("last", b::index(b::var("doubled"), b::binary(b::builtin("len", vec![b::var("doubled")]), BinaryOp::Subtract, b::num(1.0))))])),
        ],
        Scenario::HeapVariableConcat => vec![
            b::assign("other", b::list(vec![b::num(1.0), b::num(2.0)])),
            b::assign("acc", b::list(vec![])),
            b::for_in("n", b::builtin("range", vec![b::num(0.0), b::num(300.0)]), b::block(vec![b::assign("acc", b::binary(b::var("acc"), BinaryOp::Add, b::var("other")))])),
            b::finish(b::record(vec![("total", b::builtin("len", vec![b::var("acc")])), ("head", b::index(b::var("acc"), b::num(0.0)))])),
        ],
        Scenario::HeapShallowChainMutation => vec![
            b::assign("tree", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("leaf", b::list(vec![b::num(0.0)]))]))]))]))]))]))]))])),
            b::for_in("n", b::builtin("range", vec![b::num(0.0), b::num(150.0)]), b::block(vec![b::assign_path("tree", vec![b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("leaf")], b::list(vec![b::var("n")]))])),
            b::finish(b::record(vec![("leaf", b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::var("tree"), "next"), "next"), "next"), "next"), "next"), "next"), "leaf"))])),
        ],
        Scenario::HeapDeepChainMutation24 => vec![
            b::assign("tree", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("next", b::record(vec![("leaf", b::list(vec![b::num(0.0)]))]))]))]))]))]))]))]))]))]))]))]))]))]))]))]))]))]))]))]))]))]))]))]))]))])),
            b::for_in("n", b::builtin("range", vec![b::num(0.0), b::num(150.0)]), b::block(vec![b::assign_path("tree", vec![b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("next"), b::field_step("leaf")], b::list(vec![b::var("n")]))])),
            b::finish(b::record(vec![("leaf", b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::field(b::var("tree"), "next"), "next"), "next"), "next"), "next"), "next"), "next"), "next"), "next"), "next"), "next"), "next"), "next"), "next"), "next"), "next"), "next"), "next"), "next"), "next"), "next"), "next"), "next"), "next"), "leaf"))])),
        ],
    }
}
