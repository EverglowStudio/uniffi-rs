/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Self-contained runtime delivery API.
//!
//! The source is an ordinary ECMAScript module embedded by this crate.  A
//! generator receives bytes through [`runtime_source`] and does not need to
//! know where this package is checked out.

pub const RUNTIME_SOURCE: &str = include_str!("runtime.js");

pub fn runtime_source() -> &'static str {
    RUNTIME_SOURCE
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeFile {
    pub path: &'static str,
    pub source: &'static str,
}

pub fn runtime_files() -> [RuntimeFile; 1] {
    [RuntimeFile {
        path: "shared/uniffi_runtime.js",
        source: RUNTIME_SOURCE,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn source_is_owned_by_this_crate_and_plain_ecmascript() {
        assert!(RUNTIME_SOURCE.contains("export class BackendSession"));
        assert!(RUNTIME_SOURCE.contains("invokeSync(operationId, args)"));
        assert!(!RUNTIME_SOURCE.contains("typescript/src"));
        assert!(!RUNTIME_SOURCE.contains("../../../"));
        assert!(!RUNTIME_SOURCE.contains("typescript_src_dir"));
    }

    #[test]
    fn delivery_path_is_deterministic() {
        assert_eq!(runtime_files()[0].path, "shared/uniffi_runtime.js");
    }

    #[test]
    fn node_runtime_regressions() {
        let root = std::env::temp_dir().join(format!(
            "uniffi-runtime-node-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("create node test directory");
        fs::write(root.join("package.json"), "{\"type\":\"module\"}")
            .expect("write node package metadata");
        fs::write(root.join("runtime.js"), RUNTIME_SOURCE).expect("write runtime source");
        fs::write(
            root.join("check.mjs"),
            include_str!("../tests/runtime-regression.mjs"),
        )
        .expect("write node regression script");

        let output = Command::new("node")
            .arg("check.mjs")
            .current_dir(&root)
            .output()
            .expect("run node runtime regression");
        let _ = fs::remove_dir_all(&root);
        print!("{}", String::from_utf8_lossy(&output.stdout));
        assert!(
            output.status.success(),
            "node runtime regression failed: {}{}{}",
            String::from_utf8_lossy(&output.stderr),
            if output.stdout.is_empty() {
                ""
            } else {
                "\nstdout: "
            },
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
