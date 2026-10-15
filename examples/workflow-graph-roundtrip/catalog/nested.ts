export async function file_emails() {
  const emails = await gmail.list_recent({ count: 3 });
  const lists = ["today", "this week"];
  let filed = 0;
  for (const list of lists) {
    for (const email of emails) {
      await display.add_item({ list, item: email.subject });
      filed = filed + 1;
    }
  }
  return { filed };
}
