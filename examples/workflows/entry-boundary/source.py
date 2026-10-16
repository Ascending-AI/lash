async def arrow(value: int):
    return value

async def declared():
    return "top"

if True:
    async def local():
        return "block"
