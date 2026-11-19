// Equal-work TypeScript twin of array_read.py: the same ten appended elements
// 0..9 and the same ten separate read-and-add statements per trip, so its
// instruction secant divides by the Python row's Monty reference.
function benchmark(n) {
    const a = [];
    for (let i = 0; i < 10; i++) {
        a.push(i);
    }
    let result = 0;
    for (let j = 0; j < n; j++) {
        result += a[0];
        result += a[1];
        result += a[2];
        result += a[3];
        result += a[4];
        result += a[5];
        result += a[6];
        result += a[7];
        result += a[8];
        result += a[9];
    }
    return result;
}
