export async function viaJs(text: string) {
  return await dataframe.DataQuanta.readText(text)
    .flatMap((line: string) => line.toLowerCase().split(/\W+/))
    .filterBy((word: string) => word !== "")
    .map((word: string) => [word, 1])
    .reduceByKeyFn((pair) => pair[0], (a, b) => [a[0], a[1] + b[1]])
    .map((pair) => [pair[0], String(pair[1])])
    .collectRows();
}

export async function viaExprs(text: string) {
  const { col, lit, count } = dataframe;
  return await dataframe.DataQuanta.readText(text)
    .flatMap((line: string) => line.toLowerCase().split(/\W+/))
    .filter(col("value").neq(lit("")))
    .reduceByKey([col("value")], [count().alias("n")])
    .sort(col("n").sort({ ascending: false }), col("value"))
    .limit(5).collectRows();
}
