/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#![cfg(all(feature = "cli", unix))]

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(command: &mut Command) {
    let output = command.output().expect("command should start");
    assert!(
        output.status.success(),
        "command failed: {command:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_executable(path: &Path, source: &str) {
    std::fs::write(path, source).unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

fn create_git_dependency(root: &Path, name: &str, value: u32) -> PathBuf {
    let repo = root.join(name);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"same\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        format!("pub fn value() -> u32 {{ {value} }}\n"),
    )
    .unwrap();
    run(Command::new("git").arg("init").arg("-q").arg(&repo));
    run(Command::new("git").arg("-C").arg(&repo).args([
        "-c",
        "user.name=UniFFI Test",
        "-c",
        "user.email=uniffi@example.invalid",
        "add",
        ".",
    ]));
    run(Command::new("git").arg("-C").arg(&repo).args([
        "-c",
        "user.name=UniFFI Test",
        "-c",
        "user.email=uniffi@example.invalid",
        "commit",
        "-qm",
        "fixture",
    ]));
    repo
}

#[test]
fn wrapper_preserves_cargo_identity_config_flags_and_wrapper_chain() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let source_a = create_git_dependency(root, "source-a", 1);
    let source_b = create_git_dependency(root, "source-b", 2);
    let app = root.join("app");
    std::fs::create_dir_all(app.join("src")).unwrap();
    std::fs::create_dir_all(app.join(".cargo")).unwrap();
    std::fs::write(
        app.join("Cargo.toml"),
        format!(
            "[package]\nname = \"identity-app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nsame_a = {{ package = \"same\", git = \"file://{}\" }}\nsame_b = {{ package = \"same\", git = \"file://{}\" }}\n",
            source_a.display(),
            source_b.display()
        ),
    )
    .unwrap();
    std::fs::write(
        app.join("src/lib.rs"),
        "#[cfg(not(from_cargo_config))]\ncompile_error!(\"Cargo config rustflags were lost\");\n\npub fn combined() -> u32 { same_a::value() + same_b::value() }\n",
    )
    .unwrap();
    std::fs::write(
        app.join(".cargo/config.toml"),
        "[build]\nrustflags = [\"--cfg\", \"from_cargo_config\"]\n",
    )
    .unwrap();

    let log = root.join("wrapper.log");
    let outer = root.join("outer-wrapper");
    let workspace = root.join("workspace-wrapper");
    write_executable(
        &outer,
        &format!(
            "#!/bin/sh\nprintf 'outer\\n' >> '{}'\nexec \"$@\"\n",
            log.display()
        ),
    );
    write_executable(
        &workspace,
        &format!(
            "#!/bin/sh\nprintf 'workspace\\n' >> '{}'\nexec \"$@\"\n",
            log.display()
        ),
    );

    let wrapper = env!("CARGO_BIN_EXE_uniffi-bindgen");
    let run_wrapped = |scenario: &str, rustflags: Option<(&str, &str)>| {
        let mut command = Command::new("cargo");
        command
            .arg("check")
            .arg("--quiet")
            .current_dir(&app)
            .env("CARGO_TARGET_DIR", root.join(format!("target-{scenario}")))
            .env("RUSTC_WRAPPER", wrapper)
            .env("RUSTC_WORKSPACE_WRAPPER", &workspace)
            .env("UNIFFI_OHOS_RUSTC_WRAPPER", "1")
            .env("UNIFFI_OHOS_INNER_RUSTC_WRAPPER", &outer)
            .env(
                "UNIFFI_OHOS_RUSTC_APPEND_ARGS",
                "--cfg\x1fshould_only_reach_ohos_targets",
            )
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS");
        if let Some((name, value)) = rustflags {
            command.env(name, value);
        }
        run(&mut command);
    };
    run_wrapped("build-config", None);

    for (scenario, config, env_flag) in [
        (
            "target-config",
            "[target.'cfg(unix)']\nrustflags = [\"--cfg\", \"from_target_config\"]\n",
            None,
        ),
        ("plain-env", "", Some(("RUSTFLAGS", "--cfg from_plain_env"))),
        (
            "encoded-env",
            "",
            Some(("CARGO_ENCODED_RUSTFLAGS", "--cfg\x1ffrom_encoded_env")),
        ),
    ] {
        let required = scenario.replace('-', "_");
        std::fs::write(
            app.join("src/lib.rs"),
            format!(
                "#[cfg(not(from_{required}))]\ncompile_error!(\"{scenario} rustflags were lost\");\n\npub fn combined() -> u32 {{ same_a::value() + same_b::value() }}\n"
            ),
        )
        .unwrap();
        std::fs::write(app.join(".cargo/config.toml"), config).unwrap();
        run_wrapped(scenario, env_flag);
    }

    let log = std::fs::read_to_string(log).unwrap();
    assert!(log.contains("outer"));
    assert!(log.contains("workspace"));
}

#[test]
fn unix_wrapper_exec_preserves_signal_identity() {
    let temp = tempfile::tempdir().unwrap();
    let compiler = temp.path().join("signal-compiler");
    write_executable(&compiler, "#!/bin/sh\nkill -TERM $$\n");
    let status = Command::new(env!("CARGO_BIN_EXE_uniffi-bindgen"))
        .arg(&compiler)
        .arg("--target")
        .arg("aarch64-unknown-linux-ohos")
        .env("UNIFFI_OHOS_RUSTC_WRAPPER", "1")
        .env("UNIFFI_OHOS_RUSTC_APPEND_ARGS", "--cfg\x1ffrom_wrapper")
        .status()
        .unwrap();
    assert_eq!(status.signal(), Some(15));
}
