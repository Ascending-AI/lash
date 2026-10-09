const order_review = async (order: unknown) => {
  const audit = async (summary: unknown) => {
    await ledger.record({ entry: `audit:${summary.status}` });
    return summary;
  };
  const tag = (group: string, sku: string) => `${group}/${sku}`;
  const approved = [];
  const skipped = [];
  let status = "open";
  try {
    for (const group of order.groups) {
      for (const line of group.lines) {
        if (line.qty > 2) {
          const decision = await review.request({ item: tag(group.name, line.sku) });
          if (decision.approved === false) {
            throw { code: "REJECTED", sku: line.sku };
          }
          approved.push(tag(group.name, line.sku));
        } else {
          skipped.push(line.sku);
        }
        await ledger.record({ entry: tag(group.name, line.sku) });
      }
    }
    status = "reviewed";
  } catch (error) {
    status = "rejected";
    await ledger.record({ entry: "rejected" });
  }
  const audit_run = await processes.start({ definition: audit, args: { summary: { status } } });
  const audited = await audit_run;
  return { status, approved, skipped, audited };
};
