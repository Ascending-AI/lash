import asyncio


async def digest(topics: list[dict]) -> dict:
    seen = 0
    for topic in topics:
        await asyncio.sleep(7)
        if topic["urgent"]:
            await asyncio.sleep(1)
        else:
            await asyncio.sleep(2)
        seen = seen + 1
    return {"seen": seen}
