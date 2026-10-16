export async function demo(flag: boolean) {
  if (flag) { return await echo("yes"); }
  return "no";
}
