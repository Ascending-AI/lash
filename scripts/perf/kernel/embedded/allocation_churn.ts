const kept = [];
for (let n = 0; n < 400; n++) {
  const scratch = { index: n, pair: [n, n + 1], label: "row-" + String(n) };
  if (n % 40 === 0) { kept.push(scratch); }
}
const result = kept;
