#: Give back the number it is given
async def arrow(value: int):
    return value
#: Answer from the top level
async def declared():
    return "top"
def main():
    "use kernel"
    local = k.absent
    arrow = declared.arrow
    kernel_name_1 = declared.kernel_name_1
    if True:
        def local():
            return "block"
    else:
        pass
