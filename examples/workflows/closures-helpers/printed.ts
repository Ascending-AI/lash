export async function demo(items) {
  let t5 = async function(this2, args2) {
    let t1 = args2;
    if (helper_list_len(args2) < 1.0) {
      t1 = await helper_ts_pad(args2, 1.0);
    }
    let item = t1[0.0];
    let t4 = await helper_ts_property_length(item);
    return t4;
  };
  let size = ["ts.function", "", "size", 1.0, t5, {}] as const;
  let t6 = items;
  let t7 = helper_same(helper_kind(t6), "list");
  if (t7) {
  } else {
    let t8 = await helper_ts_array_is(t6);
    if (t8) {
    } else {
      throw helper_error_new("type_error", "expected an array", null);
    }
  }
  let t9 = undefined;
  if (t7) {
    let t10 = [size];
    t9 = await helper_ts_array_map(t6, t10);
  } else {
    let t12 = await helper_ts_member_map(t6);
    let t13 = [size];
    let t14 = helper_ts_callable(t12);
    t9 = await (t14)(t6, t13);
  }
  return k.returnVia(t9, helper_ts_promise_adopt);
}
async function main() {
  let demo = declared.demo;
}
