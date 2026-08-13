// Adapted from test/language/statements/try and statements/throw completion cases.
function replacement(): number {
  try {
    throw 1;
  } catch (error) {
    return error;
  } finally {
    return 2;
  }
}
finish(replacement() === 2);
