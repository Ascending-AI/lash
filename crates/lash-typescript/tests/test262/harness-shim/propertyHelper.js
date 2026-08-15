// Minimal Lash-dialect replacement for test262 harness/propertyHelper.js.
// Property descriptors and prototype mutation are outside the current dialect,
// so no selected passing test may call this shim. Keeping a callable definition
// makes an accidental include fail at execution with an explicit message.
function verifyProperty() {
  throw "propertyHelper.js is outside the current Lash TypeScript dialect";
}
