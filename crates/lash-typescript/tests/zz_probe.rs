use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, State, Value,
};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityResult::Value(Value::Null)),
            _ => Err(ExecutionHostError::new("unsupported")),
        }
    }
}

fn globals(source: &str) -> Vec<String> {
    let program = lash_typescript::compile(source).expect("compile");
    let mut state = State::new();
    match futures::executor::block_on(lashlang::execute(&program, &mut state, &Host)) {
        Ok(ExecutionOutcome::Finished(_)) => {}
        other => println!("  (outcome {other:?})"),
    }
    state
        .snapshot()
        .globals()
        .iter()
        .map(|(name, _)| name.to_string())
        .collect()
}

#[test]
fn probe_globals() {
    for source in [
        "let r = 'x'; try { throw 'boom'; } catch (e) { r = e; } finish(r);",
        "{ const a = 1; } finish('done');",
        "if (1) { const b = 2; } finish('done');",
        "const f = () => { const c = 3; return c; }; finish(`${f()}`);",
        "const g = function self(n: number): number { return n; }; finish(`${g(1)}`);",
        "while (0) { const d = 4; } finish('done');",
        "try { const t = 1; } finally { const u = 2; } finish('done');",
    ] {
        println!("{source}\n  -> {:?}", globals(source));
    }
}

#[test]
fn probe_ceilings() {
    let shapes: Vec<(&str, Box<dyn Fn(usize) -> String>)> = vec![
        (
            "grouping parens",
            Box::new(|n| format!("const x = {}1{};", "(".repeat(n), ")".repeat(n))),
        ),
        (
            "array literal",
            Box::new(|n| format!("const x = {}1{};", "[".repeat(n), "]".repeat(n))),
        ),
        (
            "object literal",
            Box::new(|n| format!("const x = {}1{};", "{ a: ".repeat(n), " }".repeat(n))),
        ),
        (
            "statement block",
            Box::new(|n| format!("{}const x = 1;{}", "{".repeat(n), "}".repeat(n))),
        ),
        (
            "if block",
            Box::new(|n| format!("{}const x = 1;{}", "if (1) {".repeat(n), "}".repeat(n))),
        ),
        (
            "nested call",
            Box::new(|n| {
                format!(
                    "const f = (a: number): number => a; const x = {}1{};",
                    "f(".repeat(n),
                    ")".repeat(n)
                )
            }),
        ),
        (
            "postfix call chain",
            Box::new(|n| {
                format!(
                    "const f = (a: number): number => a; const x = f{};",
                    "(1)".repeat(n)
                )
            }),
        ),
        (
            "postfix index chain",
            Box::new(|n| format!("const a = [1]; const x = a{};", "[0]".repeat(n))),
        ),
        (
            "prefix !",
            Box::new(|n| format!("const x = {}1;", "!".repeat(n))),
        ),
        (
            "binary terms",
            Box::new(|n| format!("const x = 1{};", "+1".repeat(n - 1))),
        ),
        (
            "member chain",
            Box::new(|n| format!("const o = {{ a: 1 }}; const x = o{};", ".a".repeat(n))),
        ),
        (
            "template holes",
            Box::new(|n| format!("const a = 1; const x = `{}`;", "${a}".repeat(n))),
        ),
        (
            "ternary",
            Box::new(|n| format!("const x = {}1;", "1?1:".repeat(n))),
        ),
        (
            "else-if branches",
            Box::new(|n| {
                let mut source = String::from("if (0) { const a = 1; }");
                for _ in 0..n {
                    source.push_str(" else if (0) { const a = 1; }");
                }
                source
            }),
        ),
        (
            "nested arrow",
            Box::new(|n| format!("const x = {}1;", "() => ".repeat(n))),
        ),
    ];
    for (name, build) in shapes {
        let mut max = 0;
        for n in 1..=100 {
            match lash_typescript::parse(&build(n)) {
                Ok(_) => max = n,
                Err(error) if error.code.as_str() == "TS_SOURCE_NESTING_LIMIT" => break,
                Err(error) => {
                    println!("{name}: n={n} other rejection {}", error.code.as_str());
                    break;
                }
            }
        }
        println!("{name}: max accepted = {max}");
    }
}
