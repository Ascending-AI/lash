async def main(tool, output):
    values = [None, False, True, 0, 1, -1, 0.0, 0.5, "", "a", [], [0], (), (0,), {}, {"k": 0}, set(), {0}]
    output([bool(value) for value in values])
    output(["yes" if value else "no" for value in values])
    output([0 or "fallback", 1 and "both", [] or {} or "last", None and 1, not [], not [0]])
    empty = []
    rounds = 0
    while empty:
        rounds += 1
    if not empty:
        output("an empty list is false")
    return [1 if "" else 2, any([0, "", None]), all([1, "a", [0]]), any([]), all([]), rounds]
