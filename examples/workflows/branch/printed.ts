export async function demo(flag) {
  let t5 = async function(this1, args1) {
    let t1 = args1;
    if (helper_list_len(args1) < 1.0) {
      t1 = await helper_ts_pad(args1, 1.0);
    }
    let flag_1 = t1[0.0];
    if (flag_1) {
      let t4 = await await tools.corpus.echo("yes");
      return t4;
    }
    return "no";
  };
  let t6 = [flag];
  let t7 = await (t5)(undefined, t6);
  if (helper_same(t7, undefined)) {
    return null;
  }
  return t7;
}
async function main() {
  let demo = declared.demo;
}
