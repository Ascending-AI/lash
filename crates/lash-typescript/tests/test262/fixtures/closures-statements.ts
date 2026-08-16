// Adapted from test/language/expressions/arrow-function and statement/if/while cases.
let total = 0;
let index = 0;
const add = (value: number): number => value + 1;
while (index < 3) {
  total = total + index;
  index = index + 1;
}
finish(total === 3 && add(2) === 3);
