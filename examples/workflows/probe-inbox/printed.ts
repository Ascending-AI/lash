/** Send a paced inbox digest */
export async function digest(items: { title: string; }[]): Promise<{ sent: number; [key: string]: never; }> {
  /** Start counting at zero */
  let sent = 0;
  /** Go through each item */
  for (const item of items) {
    /** Wait six seconds */
    await sleep(6000);
    /** Show the item's title */
    await display.showMessage({ text: item.title });
    /** Count the item as sent */
    sent = num.add(sent, 1);
  }
  /** Report how many items were sent */
  return { sent };
}
