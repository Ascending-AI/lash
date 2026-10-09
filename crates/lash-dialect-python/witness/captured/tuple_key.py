async def main(tool, output):
    grid = {}
    grid[(0, 0)] = "origin"
    grid[(1, 2)] = "a"
    grid[(1, 2.0)] = "b"
    point = (0, 0)
    output([grid[point], grid[(1, 2)], len(grid), (1, 2) in grid, (2, 1) in grid])
    for key in grid:
        x, y = key
        output((x, y, grid[key]))
    nested = {(1, (2, 3)): "deep", ("a", None): "mixed"}
    output([nested[(1, (2, 3))], nested[("a", None)]])
    try:
        grid[([1], 2)] = "no"
    except TypeError:
        output("a list is no key")
    return [grid.get((9, 9), "absent"), list(grid.items()), sorted(grid)]
