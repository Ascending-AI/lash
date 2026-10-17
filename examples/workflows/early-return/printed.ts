export async function demo(value) {
  if (helper_num_to_float(value) < 0.0) {
    return k.returnVia("negative", helper_ts_promise_adopt);
  }
  await await tools.corpus.echo((await helper_ts_boundary(value)));
  return k.returnVia("done", helper_ts_promise_adopt);
}
async function main() {
  let demo = declared.demo;
}
