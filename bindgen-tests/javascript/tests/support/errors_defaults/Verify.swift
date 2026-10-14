import Foundation

func checkFailure(_ error: SdkError) {
    switch error {
    case let .Failure(code, message, retryable, messageId):
        precondition(code == .network && message == "network unavailable")
        precondition(retryable && messageId == "m-1")
    case let .Rejected(reason): precondition(reason == "invalid")
    case .Closed: break
    }
}

@main
struct Verify {
    static func main() throws {
        for kind in UInt32(0)...2 {
            do { try fail(kind: kind); preconditionFailure("Rust must throw") }
            catch let error as SdkError { checkFailure(error) }
        }
        do { try failUnit(); preconditionFailure("Rust must throw unit") }
        catch let error as UnitError {
            guard case .Offline = error else { preconditionFailure("wrong unit") }
        }
        let state = errorState()
        checkFailure(state.error)
        checkFailure(state.optionalError!)
        precondition(isOriginal(value: state))
        precondition(isOriginal(value: echoErrors(value: state)))
        let constructed = ErrorState(
            error: .Failure(code: .network, message: "network unavailable", retryable: true, messageId: "m-1"),
            optionalError: .Closed, unitError: .Offline, optionalUnit: .Closed)
        precondition(isOriginal(value: constructed))
        let absent = echoErrors(value: ErrorState(error: state.error, optionalError: nil,
                                                 unitError: state.unitError, optionalUnit: nil))
        precondition(absent.optionalError == nil && absent.optionalUnit == nil)
        let omitted = inspectDefaults(value: Defaults(required: "r"))
        let explicitNull = inspectDefaults(value: Defaults(required: "r", label: nil))
        precondition(omitted == "r|7|record-default|<none>")
        precondition(explicitNull == "r|7|<none>|<none>")
        precondition(inspectDefaults(value: Defaults(required: "r", count: 2, label: "custom", empty: "value")) == "r|2|custom|value")
        precondition(inspectArguments(required: "r") == "r|11|argument-default|<none>")
        precondition(inspectArguments(required: "r", label: nil) == "r|11|<none>|<none>")
        print("SWIFT_ERRORS_DEFAULTS_OK", omitted, explicitNull)
    }
}
