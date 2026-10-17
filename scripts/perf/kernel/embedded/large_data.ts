const groups = {};
let total = 0;
const evens = [];
const odds = [];
for (let item = 0; item < 512; item++) {
  const key = "bucket_" + String(item % 16);
  groups[key] = (groups[key] || 0) + 1;
  total = total + item;
  if (item % 2 === 0) { evens.push(item); } else { odds.push(item); }
}
const lines = [];
for (const key of Object.keys(groups)) { lines.push(key + ":" + String(groups[key])); }
const result = { count: 512, total: total, groups: groups, evens: evens.length, odds: odds.length, summary: lines.join("|") };
