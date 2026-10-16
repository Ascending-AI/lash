async def demo(flag: bool):
    if flag:
        return await echo("yes")
    return "no"
