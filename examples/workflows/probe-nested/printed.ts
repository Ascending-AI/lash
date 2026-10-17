export async function file_emails() {
  let emails = (await await tools.gmail.list_recent((await helper_ts_boundary({count: 3.0}))));
  let lists = ["today", "this week"];
  let filed = 0.0;
  for (const list of lists) {
    for (const email of emails) {
      let t3 = undefined;
      if (helper_same(helper_kind(email), "record")) {
        if (!(helper_same(email.__ts_brand, [] as const))) {
          t3 = email.subject;
        } else {
          t3 = await helper_ts_read(email, "subject");
        }
      } else {
        t3 = await helper_ts_read(email, "subject");
      }
      await await tools.display.add_item((await helper_ts_boundary({list: list, item: t3})));
      filed = filed + 1.0;
    }
  }
  return k.returnVia({filed: filed}, helper_ts_promise_adopt);
}
async function main() {
  let file_emails = declared.file_emails;
}
