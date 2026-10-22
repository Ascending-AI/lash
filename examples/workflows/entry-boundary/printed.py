#: Give back the number it is given
async def arrow(value: int) -> int:
    return value

#: Answer from the top level
async def declared() -> str:
    return "top"
if True:
    def local():
        return "block"
