async def demo(items: list[str]):
    def size(item):
        return len(item)
    return [size(item) for item in items]
