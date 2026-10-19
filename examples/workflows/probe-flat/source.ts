/** Review the inbox */
export async function demo() {
  /** Choose the opening message */
  const greeting = "Inbox review started";
  /** Show that the review started */
  await display.show_message({ text: greeting });
  /** Get the three newest emails */
  const emails = await gmail.list_recent({ count: 3 });
  /** Mark the inbox as reviewed */
  await display.set_status({ key: "inbox", value: "Reviewed three mock emails" });
  /** Give back the emails */
  return emails;
}
