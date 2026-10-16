export async function demo(flag) {
  if (flag) {
    let t1 = await await tools.corpus.echo("yes");
    return k.returnVia(t1, helper_ts_promise_adopt);
  }
  return k.returnVia("no", helper_ts_promise_adopt);
}
async function main() {
  let demo = declared.demo;
}
