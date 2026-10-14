import { errors_defaults as api } from "./ENTRY/index.js";
import { ErrorCode } from "./components/errors_defaults/index.js";
import type { Namespace, SdkError, ErrorState } from "./components/errors_defaults/index.js";

function assert(value: unknown, message: string): asserts value {
  if (!value) throw new Error(message);
}

function checkFailure(error: SdkError): void {
  assert(error.errorName === "SdkError", "Rust error name");
  switch (error.variant) {
    case "Failure":
      assert(error.data.code === ErrorCode.Network, "typed enum field");
      assert(error.data.message === "network unavailable", "payload message");
      assert(error.data.retryable && error.data.messageId === "m-1", "payload fields");
      // @ts-expect-error other variant's field must not leak into this branch
      error.data.reason;
      break;
    case "Rejected":
      assert(error.data.reason === "invalid", "rejection reason");
      // @ts-expect-error other variant's field must not leak into this branch
      error.data.retryable;
      break;
    case "Closed":
      assert(error.data === "Closed", "mixed enum unit payload");
      // @ts-expect-error unit variant has no payload fields
      error.data.code;
      break;
    default: {
      const exhaustive: never = error;
      throw new Error(`unexpected ${exhaustive}`);
    }
  }
}

// Compile-only negative checks; they are never executed by the real backend.
function invalidInputs(ns: Namespace): void {
  // @ts-expect-error no default means required field
  ns.inspectDefaults({});
  // @ts-expect-error no default means required parameter
  ns.inspectArguments();
  // @ts-expect-error payload must match its declared variant
  new ns.SdkError("bad", "Rejected", { retryable: true });
  // @ts-expect-error undeclared variant
  new ns.SdkError("bad", "Unknown", "Unknown");
  // @ts-expect-error unit payload must match the variant
  new ns.UnitError("bad", "Offline", "Closed");
  const rejected = new ns.SdkError(undefined, "Rejected", { reason: "invalid" });
  const variant: "Rejected" = rejected.variant;
  const reason: string = rejected.data.reason;
  const business: SdkError = rejected;
  // @ts-expect-error constructor returns only the requested variant
  rejected.data.retryable;
  // @ts-expect-error constructor's variant cannot be a different branch
  const failure: "Failure" = rejected.variant;
  const closed = new ns.SdkError(undefined, "Closed", "Closed");
  const unitData: "Closed" = closed.data;
  const offline = new ns.UnitError(undefined, "Offline", "Offline");
  const offlineVariant: "Offline" = offline.variant;
  const offlineData: "Offline" = offline.data;
}
void invalidInputs;

for (const kind of [0, 1, 2]) {
  let caught = false;
  try { api.fail(kind); } catch (error) {
    assert(error instanceof api.SdkError, "generated error constructor identity");
    assert(error instanceof api.UniffiError && error instanceof Error, "base error identity");
    assert(error.name === "SdkError", "error class name");
    // Exercise narrowing directly on the caught unknown after instanceof,
    // not just on a separately annotated function parameter.
    if (error.variant === "Failure") {
      assert(error.data.code === ErrorCode.Network && error.data.retryable, "caught payload narrowing");
      // @ts-expect-error rejected-only field is unavailable after instanceof + variant
      error.data.reason;
    } else if (error.variant === "Rejected") {
      assert(error.data.reason === "invalid", "caught rejected narrowing");
    } else {
      assert(error.data === "Closed", "caught unit narrowing");
    }
    assert(error.variant === ["Failure", "Rejected", "Closed"][kind], "correct Rust variant");
    checkFailure(error);
    caught = true;
  }
  assert(caught, "Rust must throw each variant");
}
try { api.failUnit(); throw new Error("did not throw"); } catch (error) {
  assert(error instanceof api.UnitError, "unit error constructor identity");
  assert(error.variant === "Offline" && error.data === "Offline", "unit data");
}
const state = api.errorState();
assert(state.error instanceof api.SdkError, "record payload error identity");
assert(state.optionalError instanceof api.SdkError, "optional payload identity");
assert(state.unitError instanceof api.UnitError, "record unit error identity");
assert(state.optionalUnit instanceof api.UnitError, "optional unit identity");
checkFailure(state.error);
checkFailure(state.optionalError);
assert(api.isOriginal(state), "Rust comparison after lowering lifted errors");
const roundtrip = api.echoErrors(state);
assert(roundtrip.error instanceof api.SdkError && roundtrip.unitError instanceof api.UnitError, "roundtrip identities");
assert(api.isOriginal(roundtrip), "second Rust roundtrip");
const constructed: ErrorState = {
  error: new api.SdkError("failure", "Failure", {
    code: "Network", message: "network unavailable", retryable: true, messageId: "m-1",
  }),
  optionalError: new api.SdkError("closed", "Closed", "Closed"),
  unitError: new api.UnitError("offline", "Offline", "Offline"),
  optionalUnit: new api.UnitError("closed", "Closed", "Closed"),
};
assert(api.isOriginal(constructed), "constructed typed errors lower to Rust");
const absent = api.echoErrors({ ...state, optionalError: null, optionalUnit: null });
assert(absent.optionalError === null && absent.optionalUnit === null, "nullable error values");
assert(!(new Error("unknown") instanceof api.SdkError), "unknown error is not business error");
for (const invoke of [
  () => Reflect.apply(api.inspectDefaults, undefined, [{}]),
  () => Reflect.apply(api.inspectArguments, undefined, []),
]) {
  let rejected = false;
  try { invoke(); } catch (error) {
    assert(error instanceof api.UniffiError && !(error instanceof api.SdkError), "invalid input retains base runtime error");
    rejected = true;
  }
  assert(rejected, "required value must not acquire an implicit default");
}
const omitted = api.inspectDefaults({ required: "r" });
const explicitNull = api.inspectDefaults({ required: "r", label: null });
assert(omitted === "r|7|record-default|<none>", "record omitted defaults");
assert(explicitNull === "r|7|<none>|<none>", "record explicit null");
assert(api.inspectDefaults({ required: "r", count: 2, label: "custom", empty: "value" }) === "r|2|custom|value", "explicit record values");
assert(api.inspectArguments("r") === "r|11|argument-default|<none>", "argument omitted defaults");
assert(api.inspectArguments("r", 11, null) === "r|11|<none>|<none>", "argument explicit null");
assert(api.inspectArguments("r", undefined, null) === "r|11|<none>|<none>", "undefined default with explicit null");
console.log("ERRORS_DEFAULTS_OK", omitted, explicitNull);
