export async function demo(flag) {
  if (flag) {
    return k.returnVia((await await tools.corpus.echo("yes")), helper_ts_promise_adopt);
  }
  return k.returnVia("no", helper_ts_promise_adopt);
}
async function main() {
  let demo = declared.demo;
}
