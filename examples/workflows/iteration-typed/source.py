#: Add up the numbers
async def demo(items: list[int]):
    #: Start the total at zero
    total = 0
    #: Go through each number
    for item in items:
        #: Add it to the total
        total += item
    #: Give back the total
    return total
