async function main() {
  let limit = 3.0;
  let shared = [1.0];
  let add = ["ts.function", "", "add", 1.0, async function(this2, args2) {
    return (await helper_ts_promise_start(async function(this1, args1) {
      let t1 = (helper_list_len(args1) < 1.0 ? (await helper_ts_pad(args1, 1.0)) : args1);
      let x = t1[0.0];
      let t4 = helper_num_add(helper_num_to_float(x), limit);
      let t3 = undefined;
      if (0.0 < helper_list_len(shared)) {
        t3 = shared[0.0];
        if (helper_same(t3, [] as const)) {
          t3 = undefined;
        }
      } else {
        t3 = await helper_ts_get_computed(shared, 0.0);
      }
      return k.returnVia(t4 + t3, helper_ts_promise_adopt);
    }, this2, helper_list_copy(args2)));
  }, {}] as const;
  await await tools.corpus.capture((await helper_ts_boundary({list: [1.0, add]}))) as KernelType<"json">;
  await helper_ts_set(shared, 0.0, 9.0);
}
