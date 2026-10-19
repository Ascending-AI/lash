#: Give back the number it is given
async def arrow(value: int):
    return value

#: Answer from the top level
async def declared():
    return "top"

if True:
    async def local():
        return "block"
