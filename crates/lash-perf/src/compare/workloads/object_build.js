// A host TypeScript cell (FIG-7372): object literals pushed onto an array.
function benchmark(n) {
    const items = [];
    for (let i = 0; i < n; i++) { items.push({ id: i, key: (i * 7919) % n }); }
    return items.length + items[n - 1].key;
}
