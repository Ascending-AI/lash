let result = 0;
let i = 0;
while (i < 1000000) {
  if (i % 2 === 0) { result = result + i; } else { result = result - i; }
  i = i + 1;
}
