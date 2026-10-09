# deliver: echo:a1; echo:b1; echo:late; sleep:30; sleep:10; sleep:20
import asyncio
trace = []
async def step(tag, pauses):
    trace.append(tag + " starts")
    for n in range(pauses):
        await asyncio.sleep(0)
        trace.append(f"{tag} resumes {n}")
    return tag
first = asyncio.create_task(step("first", 2))
second = asyncio.create_task(step("second", 1))
trace.append("main goes on")
print(await second, trace)
print(await first, await first, trace)
async def fetch(tag):
    return await echo(tag)
a = asyncio.create_task(fetch("a1"))
b = asyncio.create_task(fetch("b1"))
print(await b, await a)
async def fail(tag):
    await asyncio.sleep(0)
    raise ValueError(tag)
async def slow(tag):
    trace.append(await echo(tag))
    return tag
doomed = asyncio.create_task(fail("task failed"))
try:
    await doomed
except ValueError as error:
    print("caught", error)
try:
    await asyncio.gather(slow("late"), fail("gather failed"))
except ValueError as error:
    print("gather raised", error, trace[-1])
await asyncio.sleep(0)
never_awaited = asyncio.create_task(fail("nobody looks"))
async def nap(tag, seconds):
    await asyncio.sleep(seconds)
    trace.append(tag)
    return seconds
print(await asyncio.gather(nap("x", 0.03), nap("y", 0.01), nap("z", 0.02)), trace[-4:])
async def inner():
    return await echo("inner")
async def outer():
    return [await inner(), await inner()]
print(await outer())
tasks = [asyncio.create_task(fetch(f"t{n}")) for n in range(3)]
print(await asyncio.gather(*tasks))
async def waiter(task):
    try:
        return await task
    except asyncio.CancelledError:
        print("waiter cancelled")
        raise
sleeper = asyncio.create_task(nap("unreached", 5))
watch = asyncio.create_task(waiter(sleeper))
await asyncio.sleep(0)
await asyncio.sleep(0)
watch.cancel()
try:
    await watch
except asyncio.CancelledError:
    print("watch is cancelled")
try:
    await sleeper
except asyncio.CancelledError:
    print("and so is what it awaited")
print("end of main")
