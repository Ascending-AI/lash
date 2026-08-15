const approval = defineProcess({
  name: "approval",
  signals: { approved: null },
  run: async (request: unknown) => {
    const decision = await waitSignal("approved");
    await sleep(25);
    wake({ stage: "approved", decision });
    return { request, decision };
  }
});

const handle = start(approval, { request: { id: "req-1" } });
finish(handle);
