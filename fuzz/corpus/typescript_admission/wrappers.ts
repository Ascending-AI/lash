const x = ({v: 7} satisfies {v: number}); (x as {v: number})!.v;
