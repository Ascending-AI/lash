export async function traffic_lights() {
  await display.set_status({ key: "traffic", value: "running" });
  for (const cycle of [1, 2]) {
    await display.add_item({ list: "cycles", item: cycle });
    await display.set_light({ name: "red", state: "on" });
    await display.set_light({ name: "amber", state: "off" });
    await display.set_light({ name: "green", state: "off" });
    await sleep(350);
    await display.set_light({ name: "red", state: "off" });
    await display.set_light({ name: "amber", state: "on" });
    await sleep(350);
    await display.set_light({ name: "amber", state: "off" });
    await display.set_light({ name: "green", state: "on" });
    await display.show_message({ text: "Go" });
    await sleep(500);
  }
  await display.set_status({ key: "traffic", value: "complete" });
  return null;
}
