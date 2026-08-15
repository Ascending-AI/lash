// Minimal Lash-dialect replacement for test262 harness/sta.js.
// Test262Error is represented by its message because the current dialect has
// no user-constructible Error heap object. Selected tests only need the name
// for assertion failures; a passing test never observes the representation.
function Test262Error(message) {
  return message ?? "Test262Error";
}

function $DONOTEVALUATE() {
  throw "Test262: this statement must not be evaluated";
}
