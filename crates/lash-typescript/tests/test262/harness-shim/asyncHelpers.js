// Lash-dialect rendering of Test262's harness/asyncHelpers.js. Upstream
// settles `$DONE` through `testFunc().then(...)`; the dialect has no promise
// chaining, so the helper awaits the test function instead, which settles
// `$DONE` on the same fulfilment or rejection. `assert.throwsAsync` is not
// rendered: no selected test calls it, and the dialect's `assert` record
// cannot be extended after assert.js defines it.
async function asyncTest(testFunc) {
  if (typeof testFunc !== "function") {
    $DONE(Test262Error("asyncTest called with non-function argument"));
    return;
  }
  try {
    await testFunc();
  } catch (error) {
    $DONE(error);
    return;
  }
  $DONE();
}
