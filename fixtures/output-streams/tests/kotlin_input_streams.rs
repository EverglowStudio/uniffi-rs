/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#[test]
fn kotlin_input_streams() -> uniffi::deps::anyhow::Result<()> {
    uniffi::kotlin_test::run_test(
        env!("CARGO_TARGET_TMPDIR"),
        env!("CARGO_PKG_NAME"),
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/bindings/test_input_streams.kts"
        ),
    )
}
