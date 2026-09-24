// Lash-dialect rendering of Test262's harness/propertyHelper.js.
//
// Upstream checks each expected attribute twice: against the descriptor that
// `Object.getOwnPropertyDescriptor` reports, and against the behaviour the
// attribute defines (a write to a writable property takes effect; an
// enumerable property is visited by `for...in`; a configurable property can be
// deleted). The dialect has no descriptor reflection, so this helper performs
// the behavioural half only, with upstream's probes, order, side effects and
// messages. An attribute is never assumed: every one the test names is
// exercised on the object itself.
function __test262HasOwn(obj, name) {
  return Object.hasOwn(obj, name);
}

function isSameValue(a, b) {
  if (a === 0 && b === 0) {
    return 1 / a === 1 / b;
  }
  if (a !== a && b !== b) {
    return true;
  }
  return a === b;
}

function isConfigurable(obj, name) {
  try {
    delete obj[name];
  } catch (e) {
    if (!(e instanceof TypeError)) {
      throw Test262Error("Expected TypeError, got " + formatSimpleValue(e));
    }
  }
  return !__test262HasOwn(obj, name);
}

function isEnumerable(obj, name) {
  let stringCheck = false;
  if (typeof name === "string") {
    for (const x in obj) {
      if (x === name) {
        stringCheck = true;
        break;
      }
    }
  } else {
    stringCheck = true;
  }
  return stringCheck && __test262HasOwn(obj, name);
}

function isWritable(obj, name, verifyProp, value, valueGiven) {
  const unlikelyValue =
    Array.isArray(obj) && name === "length" ? 4294967295 : "unlikelyValue";
  let newValue = value || unlikelyValue;
  const hadValue = __test262HasOwn(obj, name);
  const oldValue = obj[name];
  if (!valueGiven && newValue === oldValue) {
    newValue = newValue + "2";
  }
  try {
    obj[name] = newValue;
  } catch (e) {
    if (!(e instanceof TypeError)) {
      throw Test262Error("Expected TypeError, got " + formatSimpleValue(e));
    }
  }
  const writeSucceeded = isSameValue(obj[verifyProp || name], newValue);
  if (writeSucceeded) {
    if (hadValue) {
      obj[name] = oldValue;
    } else {
      delete obj[name];
    }
  }
  return writeSucceeded;
}

function verifyProperty(obj, name, desc, options) {
  const label = (options && options.label) || String(name);
  if (desc === undefined) {
    __test262Assert(!__test262HasOwn(obj, name), label + " descriptor should be undefined");
    return true;
  }
  __test262Assert(__test262HasOwn(obj, name), label + " should be an own property");
  assert["notSameValue"](desc, null, "The desc argument should be an object or undefined, null");
  assert["sameValue"](typeof desc, "object", "The desc argument should be an object or undefined, " + formatSimpleValue(desc));
  const names = Object.keys(desc);
  for (let i = 0; i < names.length; i++) {
    __test262Assert(
      names[i] === "value" || names[i] === "writable" || names[i] === "enumerable" ||
        names[i] === "configurable" || names[i] === "get" || names[i] === "set",
      "Invalid descriptor field: " + names[i]
    );
  }
  const originalValue = obj[name];
  const failures = [];
  if (__test262HasOwn(desc, "value")) {
    if (!isSameValue(desc.value, obj[name])) {
      failures.push(label + " value should be " + formatSimpleValue(desc.value));
    }
  }
  if (__test262HasOwn(desc, "enumerable") && desc.enumerable !== undefined) {
    if (desc.enumerable !== isEnumerable(obj, name)) {
      failures.push(label + " descriptor should " + (desc.enumerable ? "" : "not ") + "be enumerable");
    }
  }
  if (__test262HasOwn(desc, "writable") && desc.writable !== undefined) {
    if (desc.writable !== isWritable(obj, name)) {
      failures.push(label + " descriptor should " + (desc.writable ? "" : "not ") + "be writable");
    }
  }
  if (__test262HasOwn(desc, "configurable") && desc.configurable !== undefined) {
    if (desc.configurable !== isConfigurable(obj, name)) {
      failures.push(label + " descriptor should " + (desc.configurable ? "" : "not ") + "be configurable");
    }
  }
  if (failures.length) {
    __test262Assert(false, failures.join("; "));
  }
  if (options && options.restore && !__test262HasOwn(obj, name)) {
    obj[name] = originalValue;
  }
  return true;
}

function verifyCallableProperty(obj, name, functionName, functionLength, desc, options) {
  const label = (options && options.label) || String(name);
  const value = obj && obj[name];
  assert["sameValue"](typeof value, "function", label + " should be a function");
  const resolved =
    desc === undefined
      ? { writable: true, enumerable: false, configurable: true, value: value }
      : desc;
  verifyProperty(obj, name, resolved, options);
  verifyProperty(value, "name", {
    value: functionName === undefined ? name : functionName,
    writable: false,
    enumerable: false,
    configurable: resolved.configurable,
  }, { label: label + " name", restore: options && options.restore });
  verifyProperty(value, "length", {
    value: functionLength,
    writable: false,
    enumerable: false,
    configurable: resolved.configurable,
  }, { label: label + " length", restore: options && options.restore });
}

function verifyEqualTo(obj, name, value) {
  if (!isSameValue(obj[name], value)) {
    throw Test262Error("Expected obj[" + String(name) + "] to equal " + formatSimpleValue(value) +
      ", actually " + formatSimpleValue(obj[name]));
  }
}

function verifyWritable(obj, name, verifyProp, value) {
  if (!isWritable(obj, name, verifyProp, value, value !== undefined)) {
    throw Test262Error("Expected obj[" + String(name) + "] to be writable, but was not.");
  }
}

function verifyNotWritable(obj, name, verifyProp, value) {
  if (isWritable(obj, name, verifyProp)) {
    throw Test262Error("Expected obj[" + String(name) + "] NOT to be writable, but was.");
  }
}

function verifyEnumerable(obj, name) {
  if (!isEnumerable(obj, name)) {
    throw Test262Error("Expected obj[" + String(name) + "] to be enumerable, but was not.");
  }
}

function verifyNotEnumerable(obj, name) {
  if (isEnumerable(obj, name)) {
    throw Test262Error("Expected obj[" + String(name) + "] NOT to be enumerable, but was.");
  }
}

function verifyConfigurable(obj, name) {
  if (!isConfigurable(obj, name)) {
    throw Test262Error("Expected obj[" + String(name) + "] to be configurable, but was not.");
  }
}

function verifyNotConfigurable(obj, name) {
  if (isConfigurable(obj, name)) {
    throw Test262Error("Expected obj[" + String(name) + "] NOT to be configurable, but was.");
  }
}

function verifyPrimordialProperty(obj, name, desc, options) {
  return verifyProperty(obj, name, desc, options);
}

function verifyPrimordialCallableProperty(obj, name, functionName, functionLength, desc, options) {
  return verifyCallableProperty(obj, name, functionName, functionLength, desc, options);
}
