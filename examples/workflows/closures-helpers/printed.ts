/** Measure each word */
export async function demo(items) {
  /** Describe how to measure one word */
  let size = ["ts.function", "", "size", 1.0, async function(this2, args2) {
    return (await (async (t1) => ((await (async (item) => ((await helper_ts_property_length(item))))(t1[0.0]))))((helper_list_len(args2) < 1.0 ? (await helper_ts_pad(args2, 1.0)) : args2)));
  }, {}] as const;
  let t5 = helper_same(helper_kind(items), "list");
  if (t5) {
  } else {
    if ((await helper_ts_array_is(items))) {
    } else {
      throw helper_error_new("type_error", "expected an array", null);
    }
  }
  let t6 = undefined;
  if (t5) {
    t6 = (await helper_ts_array_map(items, [size]));
  } else {
    let t7 = await helper_ts_member_map(items);
    t6 = (await (async (t8) => ((await (async (t9) => ((await (async (t10) => ((await (t10)(items, t9))))(helper_ts_callable(t8)))))([size]))))(t7));
  }
  /** Measure every word in the list */
  return k.returnVia(t6, helper_ts_promise_adopt);
}
async function main() {
  let demo = declared.demo;
}
