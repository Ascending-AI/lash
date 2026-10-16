async function main() {
  let limit = 3.0;
  let shared = [1.0];
  let t7 = async function(this1, args1) {
    let t1 = args1;
    if (helper_list_len(args1) < 1.0) {
      t1 = await helper_ts_pad(args1, 1.0);
    }
    let x = t1[0.0];
    let t6 = helper_num_add(helper_num_to_float(x), limit);
    let t4 = undefined;
    if (0.0 < helper_list_len(shared)) {
      t4 = shared[0.0];
      if (helper_same(t4, [] as const)) {
        t4 = undefined;
      }
    } else {
      t4 = await helper_ts_get_computed(shared, 0.0);
    }
    return k.returnVia(t6 + t4, helper_ts_promise_adopt);
  };
  let t10 = async function(this2, args2) {
    let t8 = helper_list_copy(args2);
    let t9 = await helper_ts_promise_pending();
    t9.task = spawn(helper_ts_promise_run, [t9, t7, this2, t8]);
    return t9;
  };
  let add = ["ts.function", "", "add", 1.0, t10, {}] as const;
  let t11 = {nested: {list: [1.0, add]}};
  let t12 = await helper_ts_boundary(t11);
  await await tools.corpus.echo(t12);
  let t14 = shared;
  if (0.0 <= helper_list_len(t14)) {
    t14[0.0] = 9.0;
  } else {
    await helper_ts_set(t14, 0.0, 9.0);
  }
}
