/** Try a step, and always finish up */
export async function demo() {
  /** Start with a placeholder result */
  let result = "start";
  /** Try the echo */
  try {
    /** Echo a test message and keep the reply */
    result = await await corpus.echo("try") as KernelType<"json">;
  }
  /** If the echo fails */
  catch (error) {
    /** Note that it failed */
    result = "caught";
  }
  /** Whatever happened */
  finally {
    /** Send a closing echo */
    await await corpus.echo("finally") as KernelType<"json">;
  }
  /** Give back the result */
  return k.returnVia(result, helper_ts_promise_adopt);
}
async function main() {
  let demo = declared.demo;
}
