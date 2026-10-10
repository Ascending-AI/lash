const pages = await Promise.all([
  web.fetch({ url: "https://example.test/a" }),
  web.fetch({ url: "https://example.test/b" })
]);
await control.finish({ count: pages.length, first: pages[0] });
