const approval = async (request: unknown) => {
  const decision = await waitSignal("approved");
  await sleep(25);
  await processes.emit({ value: { stage: "approved", decision } });
  return { request, decision };
};

const handle = await processes.start({
  definition: approval,
  args: { request: { id: "req-1" } }
});
finish(handle);
