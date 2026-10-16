export async function demo(name) {
  let größe = "日本 🦀";
  let t1 = helper_text_concat(größe, name);
  let t2 = await helper_ts_boundary(t1);
  let t3 = await await tools.corpus.echo(t2);
  return k.returnVia(t3, helper_ts_promise_adopt);
}
async function main() {
  let demo = declared.demo;
}
