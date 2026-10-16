async def demo(items):
    async def t5(items):
        size = k.absent
        if items is k.absent:
            await helper_py_arg_missing("demo")
        else:
            pass
        async def size(item):
            if item is k.absent:
                await helper_py_arg_missing("size")
            else:
                pass
            t1 = len(item)
            return t1
        t2 = []
        item_1 = k.absent
        for t3 in items:
            item_1 = t3
            t2[helper_list_len(t2)] = await size(item_1)
        return t2
    t6 = await t5(items)
    return t6
def main():
    demo = k.absent
    demo = declared.demo
