fileprivate struct {{ ffi_converter_name }} {
    static func lower<S>(_ sequence: S) -> UInt64 where S: AsyncSequence & Sendable, S.Element == {{ item_type|type_name }} {
        uniffiCreateInputStream(
            sequence,
            lowerNext: {{ ffi_converter_name }}Next.lower,
            lowerError: { error in
                {%- if let Some(error_type) = error_type %}
                if let typedError = error as? {{ error_type|type_name }} {
                    return {{ error_type|lower_fn }}(typedError)
                }
                {%- endif %}
                return nil
            }
        )
    }
    // Callback arguments borrow a host producer whose Rust lease owns registration.
    static func lift(_ handle: UInt64) throws -> UniFfiStream<{{ item_type|type_name }}> {
        try uniffiLiftInputStream(
            handle,
            liftNext: {{ ffi_converter_name }}Next.lift,
            errorHandler: {% if let Some(error_type) = error_type %}{{ error_type|ffi_error_converter_name }}_lift{% else %}nil{% endif %}
        )
    }
}

// Host callbacks encode Option<Item>, independently of Rust output StreamStep.
fileprivate struct {{ ffi_converter_name }}Next: FfiConverterRustBuffer {
    typealias SwiftType = {{ item_type|type_name }}?
    static func write(_ value: SwiftType, into buf: inout [UInt8]) {
        guard let value else { writeInt(&buf, Int8(0)); return }
        writeInt(&buf, Int8(1))
        {{ item_type|write_fn }}(value, into: &buf)
    }
    static func read(from buf: inout (data: Data, offset: Data.Index)) throws -> SwiftType {
        switch try readInt(&buf) as Int8 {
        case 0: return nil
        case 1: return .some(try {{ item_type|read_fn }}(from: &buf))
        default: throw UniffiInternalError.unexpectedOptionalTag
        }
    }
}
