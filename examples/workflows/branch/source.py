async def demo(flag: bool):
    if flag:
        return await corpus.echo("yes")
    return "no"
