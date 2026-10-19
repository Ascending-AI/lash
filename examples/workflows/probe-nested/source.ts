/** File emails into lists */
export async function file_emails() {
  /** Get the three newest emails */
  const emails = await gmail.list_recent({ count: 3 });
  /** Name the lists to file into */
  const lists = ["today", "this week"];
  /** Start counting at zero */
  let filed = 0;
  /** Go through each list */
  for (const list of lists) {
    /** Go through each email */
    for (const email of emails) {
      /** Add the email's subject to the list */
      await display.add_item({ list, item: email.subject });
      /** Count the email as filed */
      filed = filed + 1;
    }
  }
  /** Report how many emails were filed */
  return { filed };
}
