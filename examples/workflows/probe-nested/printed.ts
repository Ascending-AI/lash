/** File emails into lists */
export async function file_emails() {
  /** Get the three newest emails */
  let emails = (await await tools.gmail.list_recent((await helper_ts_boundary({count: 3.0}))));
  /** Name the lists to file into */
  let lists = ["today", "this week"];
  /** Start counting at zero */
  let filed = 0.0;
  /** Go through each list */
  for (const list of lists) {
    /** Go through each email */
    for (const email of emails) {
      let t3 = undefined;
      if (helper_same(helper_kind(email), "record")) {
        if (!(helper_record_contains(email, "__lash_brand"))) {
          t3 = email.subject;
        } else {
          t3 = await helper_ts_read(email, "subject");
        }
      } else {
        t3 = await helper_ts_read(email, "subject");
      }
      /** Add the email's subject to the list */
      await await tools.display.add_item((await helper_ts_boundary({list: list, item: t3})));
      /** Count the email as filed */
      filed = filed + 1.0;
    }
  }
  /** Report how many emails were filed */
  return k.returnVia({filed: filed}, helper_ts_promise_adopt);
}
async function main() {
  let file_emails = declared.file_emails;
}
