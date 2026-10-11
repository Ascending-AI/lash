import asyncio
log = []
async def child(n):
    try:
        await asyncio.sleep(1)
        print("child continued", n)
    finally:
        log.append(n)
async def both():
    await asyncio.gather(child(1), child(2))
group = asyncio.create_task(both())
await asyncio.sleep(0)
await asyncio.sleep(0)
group.cancel()
try:
    await group
except asyncio.CancelledError:
    print("cancelled", log)
await asyncio.sleep(2)
print("end", log)
