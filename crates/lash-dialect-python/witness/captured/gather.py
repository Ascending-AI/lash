import asyncio


async def main(tool, output):
    counter = {"value": 0}
    order = []
    async def worker(tag, steps):
        for step in range(steps):
            seen = counter["value"]
            reply = await tool("echo", f"{tag}{step}")
            counter["value"] = seen + 1
            order.append(reply)
        return tag * 2
    results = await asyncio.gather(worker("a", 3), worker("b", 2), worker("c", 1))
    output(order)
    output(counter["value"])
    return results
