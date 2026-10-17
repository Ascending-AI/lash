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
    let local_1 = ["ts.function", "", "local", 0.0, async function(this4, args4) {
      return (await helper_ts_promise_start(async function(this3, args3) {
        return k.returnVia("block", helper_ts_promise_adopt);
      }, this4, helper_list_copy(args4)));
    }, {}] as const;
    let alsoLocal_1 = ["ts.function", "", "alsoLocal", 0.0, async function(this6, args6) {
      return (await helper_ts_promise_start(async function(this5, args5) {
        return k.returnVia("block", helper_ts_promise_adopt);
      }, this6, helper_list_copy(args6)));
    }, {}] as const;
  }
}
