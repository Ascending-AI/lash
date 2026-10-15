export async function digest(items) {
  let sent = 0;
  for (const item of items) {
    await sleep(6000);
    await display.show_message({ text: item.title });
    sent = sent + 1;
  }
  return { sent };
}
