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
  },
  // The runner passes the expected class by name (`ReferenceError`), since
  // the dialect has no constructor values; a caught error of another class,
  // or no error at all, fails.
  throws: function(expectedName, run, message) {
    let caught = false;
    try {
      run();
    } catch (error) {
      caught = true;
      if (error === null || typeof error !== "object" || error.name !== expectedName) {
        throw message ?? "assert.throws caught another error";
      }
    }
    if (!caught) {
      throw message ?? "assert.throws caught no error";
    }
  }
};
