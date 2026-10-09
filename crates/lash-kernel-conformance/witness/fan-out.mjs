export async function main({tool, print}) {
  async function child(value) {
    print(value);
    const answer = await tool('echo', value);
    print(answer);
    return answer;
  }
  return await Promise.all([child(1), child(2)]);
}
