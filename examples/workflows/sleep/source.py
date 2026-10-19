import asyncio

#: Wait a moment, then report
async def demo():
    #: Pause briefly
    await asyncio.sleep(0.005)
    #: Say it is awake
    return "awake"
