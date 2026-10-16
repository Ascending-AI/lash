async def arrow(value):
    async def t1(value):
        if value is k.absent:
            await helper_py_arg_missing("arrow")
        else:
            pass
        return value
    t2 = await t1(value)
    return t2
async def kernel_name_1():
    def t3():
        return "top"
    t4 = await t3()
    return t4
def main():
    arrow = k.absent
    kernel_name_1 = k.absent
    local = k.absent
    arrow = declared.arrow
    kernel_name_1 = declared.kernel_name_1
    if True:
        def local():
            return "block"
    else:
        pass
