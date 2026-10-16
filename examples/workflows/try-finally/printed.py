async def demo():
    async def t8():
        result = k.absent
        result = "start"
        try:
            t4 = await tools.echo("try")
            result = await helper_py_reply(t4)
        except BaseException as t1:
            t6 = ()
            t7 = await helper_py_exc_is_exception(t1, t6)
            if t7:
                result = "caught"
            else:
                raise t1
        finally:
            t2 = await tools.echo("finally")
            await helper_py_reply(t2)
        return result
    t9 = await t8()
    return t9
def main():
    demo = k.absent
    demo = declared.demo
