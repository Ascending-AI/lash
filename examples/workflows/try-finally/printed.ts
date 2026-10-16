export async function demo() {
  let result = "start";
  try {
    result = await await tools.corpus.echo("try");
  } catch (error) {
    result = "caught";
  } finally {
    await await tools.corpus.echo("finally");
  }
  return k.returnVia(result, helper_ts_promise_adopt);
}
async function main() {
  let demo = declared.demo;
}
