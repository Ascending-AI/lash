/** Review the inbox */
export async function demo() {
  /** Choose the opening message */
  let greeting = "Inbox review started";
  /** Show that the review started */
  await await tools.display.show_message((await helper_ts_boundary({text: greeting})));
  /** Get the three newest emails */
  let emails = (await await tools.gmail.list_recent((await helper_ts_boundary({count: 3.0}))));
  /** Mark the inbox as reviewed */
  await await tools.display.set_status((await helper_ts_boundary({key: "inbox", value: "Reviewed three mock emails"})));
  /** Give back the emails */
  return k.returnVia(emails, helper_ts_promise_adopt);
}
async function main() {
  let demo = declared.demo;
}
