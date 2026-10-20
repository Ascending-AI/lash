/** Review the inbox */
export async function demo() {
  /** Choose the opening message */
  const greeting = "Inbox review started";
  /** Show that the review started */
  await display.showMessage({ text: greeting });
  /** Get the three newest emails */
  const emails = await gmail.listRecent({ count: 3 });
  /** Mark the inbox as reviewed */
  await display.setStatus({ key: "inbox", value: "Reviewed three mock emails" });
  /** Give back the emails */
  return emails;
}
