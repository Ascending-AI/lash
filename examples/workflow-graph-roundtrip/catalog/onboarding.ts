export async function onboarding() {
  await display.set_status({ key: "phase", value: "starting" });
  await sleep(400);
  await display.show_message({ text: "Welcome to the workflow graph" });
  await display.set_light({ name: "ready", state: "green" });
  await sleep(400);
  await display.set_progress({ pct: 35 });
  const approval = await host.approval({});
  await display.highlight({ target: "checklist" });
  await display.add_item({ list: "steps", item: "Approved" });
  for (let count = 0; count < 2; count++) {
    await display.add_item({ list: "steps", item: "Loop item" });
    await sleep(250);
  }
  await sleep(400);
  await display.set_progress({ pct: 100 });
  await display.set_light({ name: "complete", state: "blue" });
  return approval;
}
