async def demo():
    async def t14():
        work = k.absent
        async def work(value):
            if value is k.absent:
                await helper_py_arg_missing("work")
            else:
                pass
            t1 = value
            t2 = await tools.corpus.echo(t1)
            t3 = await helper_py_reply(t2)
            return t3
        t4 = work
        async def t6():
            t5 = await t4("a")
            return t5
        t7 = spawn(helper_py_task_run, [t6])
        t8 = work
        async def t10():
            t9 = await t8("b")
            return t9
        t11 = spawn(helper_py_task_run, [t10])
        t12 = [t7, t11]
        t13 = await helper_py_gather(t12)
        return t13
    t15 = await t14()
    return t15
async def main():
    demo = k.absent
    try:
        demo = declared.demo
    finally:
        await helper_tasks_cancel_all()
