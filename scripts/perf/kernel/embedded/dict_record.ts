const counts = {};
for (let n = 0; n < 2000; n++) {
  const key = "key_" + String(n % 64);
  counts[key] = (counts[key] || 0) + n;
}
const result = counts;
