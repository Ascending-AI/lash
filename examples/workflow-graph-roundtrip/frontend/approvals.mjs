export function approvalControl(button, submit, onError) {
  const pending = new Map();
  const settled = new Set();

  function render() {
    const request = pending.values().next().value;
    button.hidden = !request;
    button.disabled = request?.submitting ?? false;
    button.textContent =
      pending.size > 1 ? `Approve (${pending.size} pending)` : "Approve";
    button.onclick = request ? () => approve(request) : null;
  }

  async function approve(request) {
    if (pending.get(request.key) !== request || request.submitting) return;
    request.submitting = true;
    render();
    try {
      await submit(request.key);
      if (pending.get(request.key) === request) {
        pending.delete(request.key);
        settled.add(request.key);
      }
    } catch (error) {
      if (pending.get(request.key) === request) onError(error);
    } finally {
      request.submitting = false;
      render();
    }
  }

  return {
    add(key) {
      if (settled.has(key)) return;
      if (!pending.has(key)) pending.set(key, { key, submitting: false });
      render();
    },
    reset() {
      pending.clear();
      settled.clear();
      render();
    },
  };
}
