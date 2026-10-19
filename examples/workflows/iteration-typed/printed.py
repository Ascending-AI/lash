#: Add up the numbers
async def demo(
    #: The numbers to add
    items,
):
    #: Start the total at zero
    total = 0
    #: Go through each number
    for item in items:
        #: Add it to the total
        total = await helper_py_iadd(total, item)
    #: Give back the total
    return total
def main():
    demo = declared.demo
