const approval = async (request: unknown) => {
  const decision = await waitSignal("approved");
  await sleep(25);
  await processes.emit({ value: { stage: "approved", decision } });
  return { request, decision };
};

const handle = start(approval, { request: { id: "req-1" } });
finish(handle);
