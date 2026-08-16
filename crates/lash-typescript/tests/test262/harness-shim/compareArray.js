// Minimal Lash-dialect replacement for test262 harness/compareArray.js.
function compareArray(actual, expected) {
  if (actual.length !== expected.length) {
    return false;
  }
  let index = 0;
  while (index < actual.length) {
    if (!__test262SameValue(actual[index], expected[index])) {
      return false;
    }
    index = index + 1;
  }
  return true;
}
