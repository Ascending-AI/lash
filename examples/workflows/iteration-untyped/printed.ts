export async function demo(items) {
  let out = [];
  for (const t1 of items) {
    let item = t1;
    let t2 = out;
    let t3 = await helper_ts_boundary(item);
    t2[helper_list_len(t2)] = await await tools.corpus.echo(t3);
  }
  return k.returnVia(out, helper_ts_promise_adopt);
}
async function main() {
  let demo = declared.demo;
}
