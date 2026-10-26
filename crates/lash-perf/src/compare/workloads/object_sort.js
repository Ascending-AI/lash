// A host TypeScript cell (FIG-7372): object literals pushed onto an array,
// then sorted by a comparator.
function benchmark(n) {
    const items = [];
    for (let i = 0; i < n; i++) { items.push({ id: i, key: (i * 7919) % n }); }
    items.sort((a, b) => a.key - b.key);
    return items[1].id + items[n - 1].key;
}
