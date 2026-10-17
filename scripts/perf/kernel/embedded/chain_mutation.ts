const tree = { level: { rows: [[0], [1], [2]], counters: { c: 0 } } };
for (let n = 0; n < 150; n++) {
  tree.level.rows[n % 3] = [n];
  tree.level.counters["c"] = tree.level.counters["c"] + 1;
}
const result = tree;
