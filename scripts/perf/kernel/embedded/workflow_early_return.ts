// examples/workflows/early-return: pure echo replaces corpus.echo/await.
function pure_echo(value) { return value; }
function demo(value) {
  if (value < 0) { return "negative"; }
  pure_echo(value);
  return "done";
}
const result = [];
for (let n = -64; n < 64; n++) { result.push(demo(n)); }
