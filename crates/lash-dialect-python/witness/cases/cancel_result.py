import asyncio
async def quick():
    return 1
async def slow():
    await asyncio.sleep(1)
ended = asyncio.create_task(quick())
await ended
waiting = asyncio.create_task(slow())
await asyncio.sleep(0)
print(ended.cancel(), waiting.cancel())
try:
    await waiting
except asyncio.CancelledError:
    print("cancelled", waiting.cancel())
