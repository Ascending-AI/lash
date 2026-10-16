export async function demo() {
  let result = "start";
  try { result = await echo("try"); }
  catch (error) { result = "caught"; }
  finally { await echo("finally"); }
  return result;
}
