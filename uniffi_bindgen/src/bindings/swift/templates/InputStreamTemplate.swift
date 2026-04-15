fileprivate struct {{ ffi_converter_name }} {
    static func lower<S>(_ sequence: S) -> UInt64 where S: AsyncSequence, S.Element == {{ item_type|type_name }} {
        uniffiCreateInputStream(
            sequence,
            lowerNext: {{ item_type|optional_lower_fn }},
            lowerError: { error in
                if let typedError = error as? {{ error_type|type_name }} {
                    return {{ error_type|lower_fn }}(typedError)
                }
                return nil
            }
        )
    }
}
