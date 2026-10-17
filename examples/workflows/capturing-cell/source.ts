const limit = 3;
const shared = [1];
const add = async (x: number) => x + limit + shared[0];
await corpus.echo({ nested: { list: [1, add] } });
shared[0] = 9;
