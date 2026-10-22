#: Say hello
async def demo() -> str:
    #: Send a greeting and give back the reply
    return await corpus.echo("hello")
