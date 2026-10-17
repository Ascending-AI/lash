async def demo(items):
    total = 0
    for item in items:
        total = await helper_py_iadd(total, item)
    return total
def main():
    demo = declared.demo
