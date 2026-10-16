async def demo(items):
    item = k.absent
    total = k.absent
    total = 0
    for t1 in items:
        item = t1
        total = await helper_py_iadd(total, item)
    return total
def main():
    demo = k.absent
    demo = declared.demo
