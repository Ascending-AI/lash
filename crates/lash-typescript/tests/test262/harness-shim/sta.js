// Lash-dialect rendering of Test262's harness/sta.js.
//
// The dialect has no user-defined constructors, prototypes or function
// properties, so Test262Error is a callable factory for an error-shaped
// record: `name` is "Test262Error", `message` defaults to "" as upstream's
// constructor defaults it. The runner bridges `new Test262Error(...)` to a
// call and `Test262Error.thrower` to `__test262ErrorThrower`.
function Test262Error(message) {
  return { name: "Test262Error", message: message || "" };
}

function __test262ErrorThrower(message) {
  throw Test262Error(message);
}

function $DONOTEVALUATE() {
  throw "Test262: This statement should not be evaluated.";
}
