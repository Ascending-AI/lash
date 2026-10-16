async def demo():
    async def t1():
        await sleep(5.0)
        return "awake"
    t2 = await t1()
    return t2
def main():
    demo = k.absent
    demo = declared.demo
