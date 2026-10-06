#[test]
fn the_dialect_accepts_bounded_while_with_nested_for() {
    let source = r#"let pool_i = 0;
let final_ids = [];
const candidate_pools = [{ matches: ["a", "b"] }];
while (final_ids.length < 2 && pool_i < candidate_pools.length) {
  for (const m of candidate_pools[pool_i].matches) {
    final_ids = [...final_ids, m];
  }
  pool_i = pool_i + 1;
}
finish(final_ids);"#;

    let program = lash_typescript::parse(source).expect("while should parse");
    lashlang::testing::harness::try_compile_program(&program).expect("while should compile");
}
