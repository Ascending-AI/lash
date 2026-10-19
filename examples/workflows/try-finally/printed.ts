/** Try a step, and always finish up */
export async function demo() {
  /** Start with a placeholder result */
  let result = "start";
  /** Try the echo */
  try {
    /** Echo a test message and keep the reply */
    result = await await tools.corpus.echo("try");
  }
  /** If the echo fails */
  catch (error) {
    /** Note that it failed */
    result = "caught";
  }
  /** Whatever happened */
  finally {
    /** Send a closing echo */
    await await tools.corpus.echo("finally");
  }
  /** Give back the result */
  return k.returnVia(result, helper_ts_promise_adopt);
}
async function main() {
  let demo = declared.demo;
}
