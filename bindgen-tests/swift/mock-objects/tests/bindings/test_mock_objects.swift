/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

import Foundation
import swift_mock_objects

// This mock adds no state to the Sendable Rust proxy.
public class TestObjectMock: TestObject, @unchecked Sendable {

    required public init(unsafeFromHandle handle: UInt64) {
        fatalError("Not supported")
    }

    public init() {
        super.init(noHandle: NoHandle.init())
    }

    // Override the "is_mock()" function

    override public func isMock() -> Bool {
        return true;
    }
}

var mocked: TestObjectMock? = TestObjectMock()

// Test that our mock object can override the functions of the real object
assert(mocked!.isMock())

// Test that we don't crash when deinitializing the mock object
mocked = nil
