async def demo():
    result = "start"
    try:
        result = await corpus.echo("try")
    except Exception:
        result = "caught"
    finally:
        await corpus.echo("finally")
    return result
