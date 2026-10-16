export async function demo(items) {
  let t5 = async function(this1, args1) {
    let t1 = args1;
    if (helper_list_len(args1) < 1.0) {
      t1 = await helper_ts_pad(args1, 1.0);
    }
    let items_1 = t1[0.0];
    let total = 0.0;
    for (const t4 of items_1) {
      let item = t4;
      total = helper_num_add(total, helper_num_to_float(item));
    }
    return total;
  };
  let t6 = [items];
  let t7 = await (t5)(undefined, t6);
  if (helper_same(t7, undefined)) {
    return null;
  }
  return t7;
}
async function main() {
  let demo = declared.demo;
}
