async def main():
    add = k.absent
    limit = k.absent
    shared = k.absent
    limit = 3
    shared = [1]
    async def add(x):
        if x is k.absent:
            await helper_py_arg_missing("add")
        else:
            pass
        t2 = await helper_py_add(x, limit)
        t3 = await helper_py_getitem(shared, 0)
        t4 = await helper_py_add(t2, t3)
        return t4
    t5 = [1, add]
    t6 = {"list": t5}
    t7 = {"nested": t6}
    t8 = await tools.corpus.echo(t7)
    await helper_py_reply(t8)
    await helper_py_setitem(shared, 0, 9)
