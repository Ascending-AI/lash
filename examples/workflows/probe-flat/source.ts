export async function demo() {
  const greeting = "Inbox review started";
  await display.show_message({ text: greeting });
  const emails = await gmail.list_recent({ count: 3 });
  await display.set_status({ key: "inbox", value: "Reviewed three mock emails" });
  return emails;
}
