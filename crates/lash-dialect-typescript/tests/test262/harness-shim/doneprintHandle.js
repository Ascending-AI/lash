// Lash-dialect rendering of Test262's harness/doneprintHandle.js. `print` is
// the dialect's own print primitive, which the runner observes.
function $DONE(error) {
  if (error) {
    if (typeof error === "object" && error !== null && "name" in error) {
      print("Test262:AsyncTestFailure:" + error.name + ": " + error.message);
    } else {
      print("Test262:AsyncTestFailure:Test262Error: " + String(error));
    }
  } else {
    print("Test262:AsyncTestComplete");
  }
}
