    /// Await business cleanup and release this wrapper's Rust reference once.
    /// Cancellation of an individual waiter does not cancel the shared cleanup.
    open func {{ meth.name()|fn_name }}() async throws {
        let task = try uniffiLifecycleLock.withLock {
            if let task = uniffiCloseTask { return task }
            guard handle != 0 else { throw UniffiInternalError.objectClosed }
            let uniffiCloseHandle = try rustCall { {{ obj.ffi_object_clone().name() }}(handle, $0) }
            let task = Task.detached { [self] in
                defer { uniffiReleaseHandle() }
                {% call swift::call_async(meth, false, true) %}{% endcall %}
            }
            uniffiCloseTask = task
            return task
        }
        try await task.value
    }
