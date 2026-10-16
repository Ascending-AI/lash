export async function demo(items) {
  let t8 = async function(this1, args1) {
    let t1 = args1;
    if (helper_list_len(args1) < 1.0) {
      t1 = await helper_ts_pad(args1, 1.0);
    }
    let items_1 = t1[0.0];
    let out = [];
    for (const t4 of items_1) {
      let item = t4;
      let t5 = out;
      let t6 = await helper_ts_boundary(item);
      t5[helper_list_len(t5)] = await await tools.echo(t6);
    }
    return out;
  };
  let t9 = [items];
  let t10 = await (t8)(undefined, t9);
  if (helper_same(t10, undefined)) {
    return null;
  }
  return t10;
}
async function main() {
  let demo = declared.demo;
}
