export async function demo() {
  let result = "start";
  try { result = await corpus.echo("try"); }
  catch (error) { result = "caught"; }
  finally { await corpus.echo("finally"); }
  return result;
}
