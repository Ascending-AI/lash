export async function demo() {
  let t1 = async function(this1, args1) {
    await await sleep(5.0);
    return "awake";
  };
  let t2 = [];
  let t3 = await (t1)(undefined, t2);
  if (helper_same(t3, undefined)) {
    return null;
  }
  return t3;
}
async function main() {
  let demo = declared.demo;
}
