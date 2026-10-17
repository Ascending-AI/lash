export async function demo(value) {
  let t6 = async function(this1, args1) {
    let t1 = args1;
    if (helper_list_len(args1) < 1.0) {
      t1 = await helper_ts_pad(args1, 1.0);
    }
    let value_1 = t1[0.0];
    if (helper_num_to_float(value_1) < 0.0) {
      return "negative";
    }
    let t4 = await helper_ts_boundary(value_1);
    await await tools.corpus.echo(t4);
    return "done";
  };
  let t7 = [value];
  let t8 = await (t6)(undefined, t7);
  if (helper_same(t8, undefined)) {
    return null;
  }
  return t8;
}
async function main() {
  let demo = declared.demo;
}
