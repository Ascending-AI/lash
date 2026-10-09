import asyncio


async def main(tool, output):
    log = []
    async def job(tag, pauses):
        try:
            for pause in range(pauses):
                await asyncio.sleep(0)
            log.append(await tool("echo", tag + ":work"))
            return "finished"
        finally:
            log.append(await tool("echo", tag + ":cleanup"))
            output(tag + " cleaned up")
    try:
        await job("a", 0)
    except Exception as error:
        output(["the failure passed through the cleanup", str(error)])
    task = asyncio.create_task(job("b", 5))
    await asyncio.sleep(0)
    await asyncio.sleep(0)
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        output("the task was cancelled")
    output(await job("c", 1))
    return log
