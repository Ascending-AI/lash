export async function arrow(value) {
  return k.returnVia(value, helper_ts_promise_adopt);
}
export async function kernel_name_1() {
  return k.returnVia("top", helper_ts_promise_adopt);
}
async function main() {
  let arrow = declared.arrow;
  let kernel_name_1 = declared.kernel_name_1;
  if (true) {
    let t1 = async function(this3, args3) {
      return k.returnVia("block", helper_ts_promise_adopt);
    };
    let t4 = async function(this4, args4) {
      let t2 = helper_list_copy(args4);
      let t3 = await helper_ts_promise_pending();
      t3.task = spawn(helper_ts_promise_run, [t3, t1, this4, t2]);
      return t3;
    };
    let local_1 = ["ts.function", "", "local", 0.0, t4, {}] as const;
    let t5 = async function(this5, args5) {
      return k.returnVia("block", helper_ts_promise_adopt);
    };
    let t8 = async function(this6, args6) {
      let t6 = helper_list_copy(args6);
      let t7 = await helper_ts_promise_pending();
      t7.task = spawn(helper_ts_promise_run, [t7, t5, this6, t6]);
      return t7;
    };
    let alsoLocal_1 = ["ts.function", "", "alsoLocal", 0.0, t8, {}] as const;
  }
}
