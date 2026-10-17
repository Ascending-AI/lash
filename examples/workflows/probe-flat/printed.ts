export async function demo() {
  let greeting = "Inbox review started";
  await await tools.display.show_message((await helper_ts_boundary({text: greeting})));
  let emails = (await await tools.gmail.list_recent((await helper_ts_boundary({count: 3.0}))));
  await await tools.display.set_status((await helper_ts_boundary({key: "inbox", value: "Reviewed three mock emails"})));
  return k.returnVia(emails, helper_ts_promise_adopt);
}
async function main() {
  let demo = declared.demo;
}
