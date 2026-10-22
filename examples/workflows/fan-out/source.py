import asyncio

#: Echo two values at once
async def demo() -> list[str]:
    #: Describe how to echo one value
    async def work(value: str):
        return await corpus.echo(value)
    #: Echo "a" and "b" side by side and wait for both
    return await asyncio.gather(work("a"), work("b"))
