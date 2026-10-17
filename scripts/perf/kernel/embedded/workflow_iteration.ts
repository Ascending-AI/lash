// examples/workflows/iteration-untyped: pure echo replaces corpus.echo/await.
function pure_echo(item) { return item; }
function demo(items) {
  const out = [];
  for (const item of items) { out.push(pure_echo(item)); }
  return out;
}
const items = [];
for (let n = 0; n < 128; n++) { items.push("item-" + String(n)); }
const result = demo(items);
