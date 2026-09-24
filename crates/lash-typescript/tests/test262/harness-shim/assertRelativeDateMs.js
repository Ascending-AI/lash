// Lash-dialect rendering of Test262's harness/assertRelativeDateMs.js.
// Upstream subtracts `date.getTimezoneOffset()`. The dialect's local time
// zone is UTC (local-time Date construction is UTC-pinned, see the crate
// README), so that offset is zero and the comparison is of the time value.
function assertRelativeDateMs(date, expectedMs) {
  const actualMs = date.valueOf();
  if (actualMs !== expectedMs) {
    throw Test262Error(
      "Expected " + String(actualMs) + " to be " + expectedMs + " milliseconds from the Unix epoch"
    );
  }
}
