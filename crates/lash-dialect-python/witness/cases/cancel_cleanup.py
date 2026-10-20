import asyncio
log = []
async def job(tag):
    try:
        log.append(await host.echo(tag + ":work"))
        await host.echo(tag + ":more")
        return "finished"
    finally:
        log.append(await host.echo(tag + ":cleanup"))
        print(tag, "cleaned up")
task = asyncio.create_task(job("a"))
await host.echo("main:first")
task.cancel()
try:
    print(await task)
except asyncio.CancelledError:
    print("the task was cancelled")
print(log)
done = asyncio.create_task(job("b"))
print(await done, log)
leftover = asyncio.create_task(job("c"))
await host.echo("main:last")
print("main ends")
