async def demo(items):
    async def t3(items):
        item = k.absent
        total = k.absent
        if items is k.absent:
            await helper_py_arg_missing("demo")
        else:
            pass
        total = 0
        for t1 in items:
            item = t1
            total = await helper_py_iadd(total, item)
        return total
    t4 = await t3(items)
    return t4
def main():
    demo = k.absent
    demo = declared.demo
