/** Try a step, and always finish up */
export async function demo(): Promise<string> {
  /** Start with a placeholder result */
  let result = "start";
  /** Try the echo */
  try {
    /** Echo a test message and keep the reply */
    result = await corpus.echo("try");
  }
  /** If the echo fails */
  catch (error) {
    /** Note that it failed */
    result = "caught";
  }
  /** Whatever happened */
  finally {
    /** Send a closing echo */
    await corpus.echo("finally");
  }
  /** Give back the result */
  return result;
}
