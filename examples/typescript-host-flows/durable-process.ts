const approval_process = async (request: unknown) => {
  const decision = await host.approval({ request });
  await sleep(25);
  return { request, decision };
};

const handle = await processes.start({
  definition: approval_process,
  args: { request: { id: "req-1" } }
});
finish(handle);
