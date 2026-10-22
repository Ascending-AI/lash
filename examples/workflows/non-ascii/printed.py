#: Grüße aus 日本 🦀
async def demo(name: str) -> str:
    #: Pick the greeting: 日本 🦀
    größe = "日本 🦀"
    #: Echo the greeting with the name
    return await corpus.echo(größe + name)
