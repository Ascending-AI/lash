export async function demo(items) {
  let t19 = async function(this1, args1) {
    let t1 = args1;
    if (helper_list_len(args1) < 1.0) {
      t1 = await helper_ts_pad(args1, 1.0);
    }
    let items_1 = t1[0.0];
    let t8 = async function(this2, args2) {
      let t4 = args2;
      if (helper_list_len(args2) < 1.0) {
        t4 = await helper_ts_pad(args2, 1.0);
      }
      let item = t4[0.0];
      let t7 = await helper_ts_property_length(item);
      return t7;
    };
    let size = ["ts.function", "", "size", 1.0, t8, {}] as const;
    let t9 = items_1;
    let t10 = helper_same(helper_kind(t9), "list");
    if (t10) {
    } else {
      let t11 = await helper_ts_array_is(t9);
      if (t11) {
      } else {
        throw helper_error_new("type_error", "expected an array", null);
      }
    }
    let t12 = undefined;
    if (t10) {
      let t13 = [size];
      t12 = await helper_ts_array_map(t9, t13);
    } else {
      let t15 = await helper_ts_member_map(t9);
      let t16 = [size];
      let t17 = helper_ts_callable(t15);
      t12 = await (t17)(t9, t16);
    }
    return t12;
  };
  let t20 = [items];
  let t21 = await (t19)(undefined, t20);
  if (helper_same(t21, undefined)) {
    return null;
  }
  return t21;
}
async function main() {
  let demo = declared.demo;
}
