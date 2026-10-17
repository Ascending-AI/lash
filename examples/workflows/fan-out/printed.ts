export async function demo() {
  let t19 = async function(this1, args1) {
    let t6 = async function(this2, args2) {
      let t1 = args2;
      if (helper_list_len(args2) < 1.0) {
        t1 = await helper_ts_pad(args2, 1.0);
      }
      let value = t1[0.0];
      let t4 = await helper_ts_boundary(value);
      let t5 = await await tools.corpus.echo(t4);
      return t5;
    };
    let t9 = async function(this3, args3) {
      let t7 = helper_list_copy(args3);
      let t8 = await helper_ts_promise_pending();
      t8.task = spawn(helper_ts_promise_run, [t8, t6, this3, t7]);
      return t8;
    };
    let work = ["ts.function", "", "work", 1.0, t9, {}] as const;
    let t10 = ["a"];
    let t11 = helper_ts_callable(work);
    let t12 = await (t11)(undefined, t10);
    let t13 = ["b"];
    let t14 = helper_ts_callable(work);
    let t15 = await (t14)(undefined, t13);
    let t16 = [[t12, t15]];
    let t17 = await helper_ts_promise_all(undefined, t16);
    let t18 = await t17;
    return t18;
  };
  let t20 = [];
  let t21 = await (t19)(undefined, t20);
  if (helper_same(t21, undefined)) {
    return null;
  }
  return t21;
}
async function main() {
  let demo = declared.demo;
}
