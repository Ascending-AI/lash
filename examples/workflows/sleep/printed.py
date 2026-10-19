#: Wait a moment, then report
async def demo():
    #: Pause briefly
    await sleep(5.0)
    #: Say it is awake
    return "awake"
def main():
    demo = declared.demo
