use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, RuntimeError,
    State, Value,
};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected URL test ability")),
        }
    }
}

fn execute(source: &str) -> Result<ExecutionOutcome, RuntimeError> {
    let program = lash_typescript::compile(source)
        .unwrap_or_else(|error| panic!("compile `{source}`: {error}"));
    futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
}

fn finished(source: &str) -> Value {
    match execute(source).unwrap_or_else(|error| panic!("execute `{source}`: {error}")) {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

#[test]
fn url_parsing_getters_setters_and_identity_are_whatwg_exact() {
    assert_eq!(
        finished(
            r#"
            const u = new URL('../café?q=a%20b#old', 'https://user:pass@例え.テスト:443/a/b');
            const p = u.searchParams;
            u.protocol = 'http:';
            u.port = '80';
            u.pathname = '/x y';
            u.hash = '#new value';
            u.origin = 'https://ignored.test';
            u.searchParams = 'ignored=1';
            finish([u.href, u.origin, u.hostname, u.port, u.pathname, u.search, u.hash,
                    p === u.searchParams, u instanceof URL, p instanceof URLSearchParams,
                    JSON.stringify(u), JSON.stringify({u: u}), JSON.stringify(p)]);
            "#,
        ),
        Value::List(
            vec![
                Value::String(
                    "http://user:pass@xn--r8jz45g.xn--zckzah/x%20y?q=a%20b#new%20value".into()
                ),
                Value::String("http://xn--r8jz45g.xn--zckzah".into()),
                Value::String("xn--r8jz45g.xn--zckzah".into()),
                Value::String("".into()),
                Value::String("/x%20y".into()),
                Value::String("?q=a%20b".into()),
                Value::String("#new%20value".into()),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true),
                Value::String(
                    "\"http://user:pass@xn--r8jz45g.xn--zckzah/x%20y?q=a%20b#new%20value\"".into(),
                ),
                Value::String(
                    "{\"u\":\"http://user:pass@xn--r8jz45g.xn--zckzah/x%20y?q=a%20b#new%20value\"}"
                        .into(),
                ),
                Value::String("{}".into()),
            ]
            .into(),
        )
    );
}

#[test]
fn url_search_params_preserve_duplicates_order_form_encoding_and_live_link() {
    assert_eq!(
        finished(
            r#"
            const u = new URL('https://example.test/?b=2&a=first&a=second&space=a%20b&plus=a+b');
            const p = u.searchParams;
            p.append('a', 'third');
            p.set('b', 'two words');
            p.delete('a', 'second');
            p.sort();
            finish([p.get('a'), p.getAll('a'), p.has('a', 'third'), p.size,
                    p.toString(), u.search, u.href, p === u.searchParams]);
            "#,
        ),
        Value::List(
            vec![
                Value::String("first".into()),
                Value::List(
                    vec![Value::String("first".into()), Value::String("third".into()),].into(),
                ),
                Value::Bool(true),
                Value::Number(5.0),
                Value::String("a=first&a=third&b=two+words&plus=a+b&space=a+b".into()),
                Value::String("?a=first&a=third&b=two+words&plus=a+b&space=a+b".into()),
                Value::String(
                    "https://example.test/?a=first&a=third&b=two+words&plus=a+b&space=a+b".into(),
                ),
                Value::Bool(true),
            ]
            .into(),
        )
    );
}

#[test]
fn url_search_params_constructor_forms_iteration_and_callback_work() {
    assert_eq!(
        finished(
            r#"
            const fromString = new URLSearchParams('?x=1&x=2+y');
            const fromPairs = new URLSearchParams([['a', 1], ['a', 2]]);
            const fromObject = new URLSearchParams({b: 2, a: 1});
            const fromNull = new URLSearchParams(null);
            const copy = new URLSearchParams(fromString);
            const seen = [];
            copy.forEach((value, name) => { seen[seen.length] = name + '=' + value; });
            const direct = [];
            for (const pair of fromPairs) { direct[direct.length] = pair.join(':'); }
            finish([fromString.toString(), fromPairs.toString(), fromObject.toString(),
                    copy.toString(), seen, direct, URL.canParse('/x', 'https://e.test/'),
                    URL.canParse('not relative'), fromNull.toString(),
                    URL.canParse('https://e.test/', undefined)]);
            "#,
        ),
        Value::List(
            vec![
                Value::String("x=1&x=2+y".into()),
                Value::String("a=1&a=2".into()),
                Value::String("b=2&a=1".into()),
                Value::String("x=1&x=2+y".into()),
                Value::List(
                    vec![Value::String("x=1".into()), Value::String("x=2 y".into())].into(),
                ),
                Value::List(vec![Value::String("a:1".into()), Value::String("a:2".into())].into(),),
                Value::Bool(true),
                Value::Bool(false),
                Value::String("".into()),
                Value::Bool(true),
            ]
            .into(),
        )
    );
}

#[test]
fn invalid_urls_fail_with_the_named_diagnostic() {
    let error = execute("finish(new URL('relative only'));")
        .expect_err("an invalid URL without a base must reject");
    assert!(error.to_string().contains("TS_URL_PARSE_ERROR"), "{error}");
}

#[test]
fn url_search_params_paths_are_individually_executable() {
    for source in [
        "const p = new URLSearchParams('?x=1'); finish(p.toString());",
        "const p = new URLSearchParams('?x=1'); const q = new URLSearchParams(p); finish(q.toString());",
        "const p = new URLSearchParams('?x=1'); const a=[]; p.forEach((v,k) => { a[a.length] = k + '=' + v; }); finish(a);",
        "const p = new URLSearchParams([['a',1]]); const a=[]; for (const pair of p) { a[a.length] = pair.join(':'); } finish(a);",
        "finish(URL.canParse('/x', 'https://e.test/'));",
    ] {
        execute(source).unwrap_or_else(|error| panic!("source `{source}` failed: {error}"));
    }
}

#[test]
fn url_search_params_for_each_observes_live_appends_and_deletes() {
    assert_eq!(
        finished(
            r#"
            const appended = new URLSearchParams('a=1&b=2');
            const appendSeen = [];
            appended.forEach((value, name) => {
                appendSeen[appendSeen.length] = name;
                if (name === 'a') { appended.append('c', '3'); }
            });
            const deleted = new URLSearchParams('a=1&b=2&c=3');
            const deleteSeen = [];
            deleted.forEach((value, name) => {
                deleteSeen[deleteSeen.length] = name;
                if (name === 'a') { deleted.delete('b'); }
            });
            finish([appendSeen, deleteSeen]);
            "#,
        ),
        Value::List(
            vec![
                Value::List(
                    vec![
                        Value::String("a".into()),
                        Value::String("b".into()),
                        Value::String("c".into()),
                    ]
                    .into(),
                ),
                Value::List(vec![Value::String("a".into()), Value::String("c".into())].into(),),
            ]
            .into(),
        )
    );
}

#[test]
fn hostile_large_url_inputs_are_bounded_without_aborting() {
    assert_eq!(
        finished(
            "const u = new URL('https://example.test/' + 'x'.repeat(1000000)); finish(u.href.length);",
        ),
        Value::Number(1_000_021.0)
    );
    assert!(matches!(
        execute(
            "const u = new URL('https://example.test/' + 'x'.repeat(8388608)); finish(u.href);",
        ),
        Err(lashlang::RuntimeError::MemoryLimitExceeded { .. })
    ));
}
