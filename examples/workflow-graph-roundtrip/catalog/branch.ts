export async function demo() {
  const notify = true;
  if (notify) {
    await display.show_message({ text: "Inside the notification branch" });
  } else {
    await display.show_message({ text: "Notifications disabled" });
  }
  return null;
}
