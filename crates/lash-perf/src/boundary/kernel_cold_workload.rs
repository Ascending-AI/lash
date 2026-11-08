const SOURCE: &str = r#"kernel 1
numbers by_spelling
import 0 operation host.seed() -> Any
import 1 operation host.pause() -> Int
main {
  let xs = perform 0() as Any
  let before_read = perform 1() as Int
  let found = POINT_GET(xs, 100003)
  print found
  let before_edit = perform 1() as Int
  set xs[100003] = -1
  let after_edit = perform 1() as Int
  let edited = POINT_GET(xs, 100003)
  print edited
  finish edited
}
"#;
