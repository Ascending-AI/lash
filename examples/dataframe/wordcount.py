async def via_js(text: str):
    return await (dataframe.DataQuanta.read_text(text)
        .flat_map(lambda line: line.lower().split())
        .filter_by(lambda word: word != "")
        .map(lambda word: [word, 1])
        .reduce_by_key_fn(lambda pair: pair[0], lambda a, b: [a[0], a[1] + b[1]])
        .map(lambda pair: [pair[0], str(pair[1])])
        .collect_rows())

async def via_exprs(text: str):
    col, lit, count = dataframe.col, dataframe.lit, dataframe.count
    return await (dataframe.DataQuanta.read_text(text)
        .flat_map(lambda line: line.lower().split())
        .filter(col("value").neq(lit("")))
        .reduce_by_key([col("value")], [count().alias("n")])
        .sort(col("n").sort(ascending=False), col("value"))
        .limit(5).collect_rows())
