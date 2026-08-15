// Minimal Lash-dialect replacement for test262 harness/assert.js.
function __test262SameValue(actual, expected) {
  if (actual === expected) {
    return actual !== 0 || 1 / actual === 1 / expected;
  }
  return actual !== actual && expected !== expected;
}

const assert = {
  sameValue: function(actual, expected, message) {
    if (!__test262SameValue(actual, expected)) {
      throw message ?? "assert.sameValue failed";
    }
  },
  notSameValue: function(actual, unexpected, message) {
    if (__test262SameValue(actual, unexpected)) {
      throw message ?? "assert.notSameValue failed";
    }
  },
  compareArray: function(actual, expected, message) {
    if (!compareArray(actual, expected)) {
      throw message ?? "assert.compareArray failed";
    }
  }
};
