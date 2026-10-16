async def demo():
    result = "start"
    try:
        result = await echo("try")
    except Exception:
        result = "caught"
    finally:
        await echo("finally")
    return result
