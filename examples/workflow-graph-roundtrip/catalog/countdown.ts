export async function countdown() {
  let count = 0;
  while (true) {
    count = count + 1;
    await display.set_progress({ pct: count * 25 });
    if (count >= 4) {
      break;
    }
    await display.show_message({ text: "Tick " + count });
  }
  await display.set_status({ key: "countdown", value: "done" });
  return { count };
}
