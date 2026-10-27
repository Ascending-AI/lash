const limit = 3;
const shared = [1];
const add = async (x: number) => {
  return x + limit + shared[0];
};
await corpus.capture({ list: [1, add] });
shared[0] = 9;
