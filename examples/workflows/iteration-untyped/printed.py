#: Echo every item
async def demo(items: list[str]) -> list[str]:
    #: Start an empty list of replies
    out = []
    #: Go through each item
    for item in items:
        #: Echo the item and keep the reply
        out.append(await corpus.echo(item))
    #: Give back the replies
    return out
