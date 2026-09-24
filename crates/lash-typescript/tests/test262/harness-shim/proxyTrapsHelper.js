// Lash-dialect rendering of Test262's harness/proxyTrapsHelper.js. The
// dialect refuses reassigning a parameter (TS_ASSIGN_CONST), so the defaulted
// overrides live in a `const`.
function allowProxyTraps(overrides, label) {
  const prefix = typeof label === "string" && label.length > 0 ? label + ": " : "";
  function throwTest262Error(msg) {
    return function () {
      __test262ErrorThrower(prefix + msg);
    };
  }
  const given = overrides || {};
  return {
    getPrototypeOf: given.getPrototypeOf || throwTest262Error("[[GetPrototypeOf]] trap called"),
    setPrototypeOf: given.setPrototypeOf || throwTest262Error("[[SetPrototypeOf]] trap called"),
    isExtensible: given.isExtensible || throwTest262Error("[[IsExtensible]] trap called"),
    preventExtensions: given.preventExtensions || throwTest262Error("[[PreventExtensions]] trap called"),
    getOwnPropertyDescriptor: given.getOwnPropertyDescriptor || throwTest262Error("[[GetOwnProperty]] trap called"),
    has: given.has || throwTest262Error("[[HasProperty]] trap called"),
    get: given.get || throwTest262Error("[[Get]] trap called"),
    set: given.set || throwTest262Error("[[Set]] trap called"),
    deleteProperty: given.deleteProperty || throwTest262Error("[[Delete]] trap called"),
    defineProperty: given.defineProperty || throwTest262Error("[[DefineOwnProperty]] trap called"),
    enumerate: throwTest262Error("[[Enumerate]] trap called: this trap has been removed"),
    ownKeys: given.ownKeys || throwTest262Error("[[OwnPropertyKeys]] trap called"),
    apply: given.apply || throwTest262Error("[[Call]] trap called"),
    construct: given.construct || throwTest262Error("[[Construct]] trap called"),
  };
}
