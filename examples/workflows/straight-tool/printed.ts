export async function demo() {
  let t2 = async function(this1, args1) {
    let t1 = await await tools.corpus.echo("hello");
    return t1;
  };
  let t3 = [];
  let t4 = await (t2)(undefined, t3);
  if (helper_same(t4, undefined)) {
    return null;
  }
  return t4;
}
async function main() {
  let demo = declared.demo;
}
