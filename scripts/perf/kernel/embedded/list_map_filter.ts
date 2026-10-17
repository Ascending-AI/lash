const source = [];
for (let n = 0; n < 800; n++) { source.push(n); }
const doubled = source.map((item) => item * 2);
const result = doubled.filter((item) => item % 3 === 0);
