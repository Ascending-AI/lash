use lashlang::{AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, State, Value};
#[derive(Default)]
struct Host(std::sync::Mutex<Vec<Value>>);
impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Print(value) => {
                self.0.lock().unwrap().push(value);
                Ok(AbilityResult::Value(Value::Null))
            }
            _ => Err(ExecutionHostError::new("unexpected operation")),
        }
    }
}
#[test]
fn console_methods_follow_observation_rendering() {
    for method in ["log", "info", "warn", "error", "debug"] {
        for (args, expected) in [
            ("{ a: 1, b: [2, 3] }", r#"{"a":1,"b":[2,3]}"#),
            (r#""x", { a: 1 }"#, r#"x {"a":1}"#),
            ("[{ a: 1 }]", r#"[{"a":1}]"#),
            ("null", "null"),
            ("undefined", "undefined"),
        ] {
            let source = format!("console.{method}({args});");
            let program = lash_typescript::compile(&source).unwrap();
            let host = Host::default();
            futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &host))
                .unwrap();
            assert_eq!(
                *host.0.lock().unwrap(),
                vec![Value::String(expected.into())],
                "{source}"
            );
        }
    }
}
