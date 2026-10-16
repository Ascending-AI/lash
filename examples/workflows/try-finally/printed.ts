export async function demo() {
  let t3 = async function(this1, args1) {
    let result = "start";
    try {
      result = await await tools.echo("try");
    } catch (error) {
      result = "caught";
    } finally {
      await await tools.echo("finally");
    }
    return result;
  };
  let t4 = [];
  let t5 = await (t3)(undefined, t4);
  if (helper_same(t5, undefined)) {
    return null;
  }
  return t5;
}
async function main() {
  let demo = declared.demo;
}
