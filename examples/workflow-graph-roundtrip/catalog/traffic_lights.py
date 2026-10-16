import asyncio


async def traffic_lights():
    await display.set_status(key="traffic", value="running")
    for cycle in [1, 2]:
        await display.add_item(list="cycles", item=cycle)
        await display.set_light(name="red", state="on")
        await display.set_light(name="amber", state="off")
        await display.set_light(name="green", state="off")
        await asyncio.sleep(0.35)
        await display.set_light(name="red", state="off")
        await display.set_light(name="amber", state="on")
        await asyncio.sleep(0.35)
        await display.set_light(name="amber", state="off")
        await display.set_light(name="green", state="on")
        await display.show_message(text="Go")
        await asyncio.sleep(0.5)
    await display.set_status(key="traffic", value="complete")
    return None
