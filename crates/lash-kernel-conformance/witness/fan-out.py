import asyncio


async def main(tool, output):
    async def child(value):
        output(value)
        answer = await tool("echo", value)
        output(answer)
        return answer

    return await asyncio.gather(child(1), child(2))
