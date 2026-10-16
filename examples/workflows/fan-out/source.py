import asyncio

async def demo():
    async def work(value: str):
        return await echo(value)
    return await asyncio.gather(work("a"), work("b"))
