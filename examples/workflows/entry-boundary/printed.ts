export async function arrow(value) {
  let t4 = async function(this1, args1) {
    let t1 = args1;
    if (helper_list_len(args1) < 1.0) {
      t1 = await helper_ts_pad(args1, 1.0);
    }
    let value_1 = t1[0.0];
    return value_1;
  };
  let t5 = [value];
  let t6 = await (t4)(undefined, t5);
  if (helper_same(t6, undefined)) {
    return null;
  }
  return t6;
}
export async function kernel_name_1() {
  let t7 = async function(this2, args2) {
    return "top";
  };
  let t8 = [];
  let t9 = await (t7)(undefined, t8);
  if (helper_same(t9, undefined)) {
    return null;
  }
  return t9;
}
async function main() {
  let arrow = declared.arrow;
  let kernel_name_1 = declared.kernel_name_1;
  if (true) {
    let t10 = async function(this3, args3) {
      return "block";
    };
    let t13 = async function(this4, args4) {
      let t11 = helper_list_copy(args4);
      let t12 = await helper_ts_promise_pending();
      t12.task = spawn(helper_ts_promise_run, [t12, t10, this4, t11]);
      return t12;
    };
    let local_1 = ["ts.function", "", "local", 0.0, t13, {}] as const;
    let t14 = async function(this5, args5) {
      return "block";
    };
    let t17 = async function(this6, args6) {
      let t15 = helper_list_copy(args6);
      let t16 = await helper_ts_promise_pending();
      t16.task = spawn(helper_ts_promise_run, [t16, t14, this6, t15]);
      return t16;
    };
    let alsoLocal_1 = ["ts.function", "", "alsoLocal", 0.0, t17, {}] as const;
  }
}
