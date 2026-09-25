// Lash-dialect rendering of Test262's harness/assert.js (which, upstream, also
// defines compareArray). Every assertion keeps upstream's pass/fail semantics
// and message text. Two spellings differ, both bridged by the runner at
// ingestion: `assert(...)` is `__test262Assert(...)`, because a dialect
// function cannot also carry the `assert.*` methods; and `assert.throws`
// receives the expected class's name, because the dialect has no constructor
// values, and compares it with the caught error's `name`.
function isNegativeZero(value) {
  return value === 0 && 1 / value === -Infinity;
}

function isPrimitive(value) {
  return !value || (typeof value !== "object" && typeof value !== "function");
}

function formatIdentityFreeValue(value) {
  if (value === null) {
    return "null";
  }
  if (typeof value === "string") {
    return JSON.stringify(value);
  }
  if (typeof value === "number") {
    return isNegativeZero(value) ? "-0" : String(value);
  }
  if (typeof value === "boolean" || typeof value === "undefined") {
    return String(value);
  }
  return undefined;
}

// Upstream falls back to `String(value)`. Here an object is named by its kind
// instead, so building a failing assertion's message never runs an object's
// own toString or converts a function (TS_FUNCTION_STRING_COERCION): the
// assertion must report its failure, never a refusal raised by its message.
function formatSimpleValue(value) {
  const basic = formatIdentityFreeValue(value);
  if (basic) {
    return basic;
  }
  if (typeof value === "function") {
    return "[function]";
  }
  if (Array.isArray(value)) {
    return "[array]";
  }
  if (typeof value.name === "string" && typeof value.message === "string") {
    return value.name + ": " + value.message;
  }
  return "[object]";
}

function __test262SameValue(a, b) {
  if (a === b) {
    return a !== 0 || 1 / a === 1 / b;
  }
  return a !== a && b !== b;
}

function __test262Prefix(message) {
  return message === undefined ? "" : message + " ";
}

function __test262Assert(mustBeTrue, message) {
  if (mustBeTrue === true) {
    return;
  }
  throw Test262Error(
    message === undefined ? "Expected true but got " + formatSimpleValue(mustBeTrue) : message
  );
}

function compareArray(a, b) {
  if (b.length !== a.length) {
    return false;
  }
  for (let i = 0; i < a.length; i++) {
    if (!__test262SameValue(b[i], a[i])) {
      return false;
    }
  }
  return true;
}

function __test262FormatArray(arrayLike) {
  const parts = [];
  for (let i = 0; i < arrayLike.length; i++) {
    parts.push(formatSimpleValue(arrayLike[i]));
  }
  return "[" + parts.join(", ") + "]";
}

const assert = {
  _isSameValue: __test262SameValue,
  _toString: formatSimpleValue,
  _formatIdentityFreeValue: formatIdentityFreeValue,
  sameValue: function (actual, expected, message) {
    if (__test262SameValue(actual, expected)) {
      return;
    }
    throw Test262Error(
      __test262Prefix(message) +
        "Expected SameValue(«" + formatSimpleValue(actual) + "», «" +
        formatSimpleValue(expected) + "») to be true"
    );
  },
  notSameValue: function (actual, unexpected, message) {
    if (!__test262SameValue(actual, unexpected)) {
      return;
    }
    throw Test262Error(
      __test262Prefix(message) +
        "Expected SameValue(«" + formatSimpleValue(actual) + "», «" +
        formatSimpleValue(unexpected) + "») to be false"
    );
  },
  throws: function (expectedName, func, message) {
    if (typeof func !== "function") {
      throw Test262Error(
        "assert.throws requires two arguments: the error constructor and a function to run"
      );
    }
    let threw = false;
    try {
      func();
    } catch (thrown) {
      threw = true;
      if (typeof thrown !== "object" || thrown === null) {
        throw Test262Error(__test262Prefix(message) + "Thrown value was not an object!");
      }
      // A VM fault surfaces as a `RuntimeError`, which no ECMA program can
      // construct or expect. It propagates as itself, so the runner reports
      // the fault (or the refusal it carries) rather than a mismatched class.
      if (thrown.name === "RuntimeError") {
        throw thrown;
      }
      if (thrown.name !== expectedName) {
        throw Test262Error(
          __test262Prefix(message) + "Expected a " + expectedName + " but got a " +
            formatSimpleValue(thrown.name)
        );
      }
    }
    if (!threw) {
      throw Test262Error(
        __test262Prefix(message) + "Expected a " + expectedName +
          " to be thrown but no exception was thrown at all"
      );
    }
  },
  compareArray: function (actual, expected, message) {
    const suffix = message === undefined ? "" : String(message);
    if (isPrimitive(actual)) {
      __test262Assert(false, "Actual argument [" + formatSimpleValue(actual) + "] shouldn't be primitive. " + suffix);
    } else if (isPrimitive(expected)) {
      __test262Assert(false, "Expected argument [" + formatSimpleValue(expected) + "] shouldn't be primitive. " + suffix);
    }
    if (compareArray(actual, expected)) {
      return;
    }
    __test262Assert(
      false,
      "Actual " + __test262FormatArray(actual) + " and expected " +
        __test262FormatArray(expected) + " should have the same contents. " + suffix
    );
  },
};
