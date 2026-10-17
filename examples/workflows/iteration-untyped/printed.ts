export async function demo(items) {
  let out = [];
  for (const item of items) {
    out[helper_list_len(out)] = (await await tools.corpus.echo((await helper_ts_boundary(item))));
  }
  return k.returnVia(out, helper_ts_promise_adopt);
}
async function main() {
  let demo = declared.demo;
}
