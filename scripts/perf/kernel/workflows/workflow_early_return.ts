async function demo(value) {
  if (value < 0) { return "negative"; }
  await perf.echo(value);
  return "done";
}
const result = [];
for (let n = -64; n < 64; n++) { result.push(await demo(n)); }
