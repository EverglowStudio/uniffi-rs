/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use uniffi_meta::{
    CallbackInterfaceMetadata, FnMetadata, FnParamMetadata, MetadataGroup, MethodMetadata,
    NamespaceMetadata, ObjectMetadata, TraitKind, TraitMethodMetadata,
};

const MODULE: &str = "kotlin_async_contract";

fn stream(is_send: bool) -> Type {
    Type::Stream {
        item_type: Box::new(Type::Optional {
            inner_type: Box::new(Type::UInt32),
        }),
        error_type: None,
        is_send,
    }
}

fn input(is_send: bool) -> FnParamMetadata {
    FnParamMetadata {
        name: "values".into(),
        ty: Type::InputStream {
            item_type: Box::new(Type::Optional {
                inner_type: Box::new(Type::UInt32),
            }),
            error_type: None,
            is_send,
        },
        by_ref: false,
        optional: false,
        default: None,
    }
}

fn method(owner: &str, name: &str, is_async: bool, is_send: bool) -> MethodMetadata {
    MethodMetadata {
        module_path: MODULE.into(),
        self_name: owner.into(),
        name: name.into(),
        orig_name: None,
        is_async,
        rust_async: false,
        rust_return_is_result: false,
        rust_stream_item_is_result: false,
        inputs: vec![input(is_send)],
        return_type: Some(stream(is_send)),
        throws: None,
        takes_self_by_arc: false,
        checksum: None,
        docstring: None,
    }
}

fn bindings(callback_only: bool) -> String {
    let mut group = MetadataGroup {
        namespace: NamespaceMetadata {
            crate_name: MODULE.into(),
            name: MODULE.into(),
        },
        namespace_docstring: None,
        items: Default::default(),
    };
    if !callback_only {
        for (name, is_async) in [("sync_stream", false), ("async_stream", true)] {
            group.add_item(
                FnMetadata {
                    module_path: MODULE.into(),
                    name: name.into(),
                    orig_name: None,
                    is_async,
                    rust_async: false,
                    rust_return_is_result: false,
                    rust_stream_item_is_result: false,
                    inputs: vec![input(true)],
                    return_type: Some(stream(true)),
                    throws: None,
                    checksum: None,
                    docstring: None,
                }
                .into(),
            );
        }
        for (name, imp, is_async, is_send) in [
            ("Shared", ObjectImpl::Struct, false, true),
            ("Local", ObjectImpl::LocalStruct, true, false),
            ("Both", ObjectImpl::Trait(TraitKind::Both), true, true),
        ] {
            group.add_item(
                ObjectMetadata {
                    module_path: MODULE.into(),
                    name: name.into(),
                    orig_name: None,
                    remote: false,
                    imp,
                    docstring: None,
                }
                .into(),
            );
            group.add_item(method(name, "events", is_async, is_send).into());
            if name == "Local" {
                let mut close = method(name, "close", true, false);
                close.inputs.clear();
                close.return_type = None;
                group.add_item(close.into());
            }
        }
    }
    group.add_item(
        CallbackInterfaceMetadata {
            module_path: MODULE.into(),
            name: "Producer".into(),
            docstring: None,
        }
        .into(),
    );
    for (index, is_async) in [false, true].into_iter().enumerate() {
        let m = method(
            "Producer",
            if is_async { "async_values" } else { "values" },
            is_async,
            true,
        );
        group.add_item(
            TraitMethodMetadata {
                module_path: m.module_path,
                trait_name: m.self_name,
                index: index as u32,
                name: m.name,
                orig_name: None,
                is_async,
                rust_async: false,
                rust_return_is_result: false,
                rust_stream_item_is_result: false,
                inputs: if callback_only { vec![] } else { m.inputs },
                return_type: m.return_type,
                throws: None,
                takes_self_by_arc: false,
                checksum: None,
                docstring: None,
            }
            .into(),
        );
    }
    let mut ci = ComponentInterface::from_metadata(group).unwrap();
    ci.derive_ffi_funcs().unwrap();
    generate_bindings(
        &Config {
            package_name: Some(format!("uniffi.{MODULE}")),
            cdylib_name: Some(MODULE.into()),
            ..Config::default()
        },
        &ci,
    )
    .unwrap()
}

#[test]
fn kotlin_async_stream_declarations_compile() {
    use std::process::Command;
    for callback_only in [false, true] {
        let source = bindings(callback_only);
        assert!(source.contains("fun `values`()") || source.contains("fun `values`("));
        assert!(source.contains("suspend fun `asyncValues`("));
        assert!(source.contains("uniffiInputStreamCloseCallbackImpl,"));
        assert!(!source.contains("GlobalScope"));
        assert!(source.contains("withContext(NonCancellable)"));
        if !callback_only {
            assert!(source.contains("suspend fun `asyncStream`("));
            assert!(source.contains("override suspend fun `events`("));
            assert!(source.contains("FfiConverterInputStreamOptionalUIntInfallibleLocal"));
            assert!(source.contains("FfiConverterStreamOptionalUIntInfallibleLocal"));
        }
        if Command::new("kotlinc").arg("-version").output().is_err() {
            eprintln!("SKIP Kotlin compiler check: kotlinc unavailable");
            continue;
        }
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("AsyncContract.kt");
        std::fs::write(&file, &source).unwrap();
        let output = Command::new("kotlinc")
            .arg("-Werror")
            .arg(&file)
            .arg("-classpath")
            .arg(std::env::var("CLASSPATH").expect("Kotlin gate requires JNA/coroutines CLASSPATH"))
            .arg("-d")
            .arg(temp.path().join("contract.jar"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "callback_only={callback_only}\n{}\n{}\n{source}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn kotlin_async_completion_preserves_cleanup_errors() {
    use std::process::Command;

    let temp = tempfile::tempdir().unwrap();
    let bindings_file = temp.path().join("AsyncContract.kt");
    let test_file = temp.path().join("CompletionTest.kt");
    let jar = temp.path().join("completion.jar");
    std::fs::write(&bindings_file, bindings(false)).unwrap();
    std::fs::write(&test_file, format!("{}\n{}", include_str!("async_completion_test.kt"), include_str!("c_abi_terminal_mock.kt"))).unwrap();
    let classpath = std::env::var("CLASSPATH")
        .expect("Kotlin completion gate requires JNA/coroutines CLASSPATH");
    let output = Command::new("kotlinc")
        .arg("-Werror")
        .args([&bindings_file, &test_file])
        .arg("-classpath")
        .arg(&classpath)
        .arg("-d")
        .arg(&jar)
        .output()
        .expect("Kotlin completion gate requires kotlinc");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let output = Command::new("kotlin")
        .arg("-J-ea")
        .arg("-classpath")
        .arg(format!("{}:{classpath}", jar.display()))
        .arg("uniffi.kotlin_async_contract.CompletionTestKt")
        .output()
        .expect("Kotlin completion gate requires kotlin");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    print!("{}", String::from_utf8_lossy(&output.stdout));
}

#[test]
fn kotlin_input_close_completion_protocol() {
    let temp = tempfile::tempdir().unwrap();
    let runner = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/bindings/kotlin/gen_kotlin/stream_close_test.py");
    let output = std::process::Command::new("python3")
        .arg(runner)
        .arg("--output")
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    print!("{}", String::from_utf8_lossy(&output.stdout));
}

#[test]
fn kotlin_generated_calls_use_consuming_terminal_carriers() {
    let source = bindings(false);
    for suffix in ["u64", "rust_buffer", "void"] {
        assert!(source.contains(&format!("-> UniffiLib.ffi_{MODULE}_rust_future_complete_and_free_{suffix}(future,")));
        assert!(!source.contains(&format!("-> UniffiLib.ffi_{MODULE}_rust_future_complete_{suffix}(future,")));
    }
    assert!(source.contains("terminalOwnsHandle = true"));
    assert!(source.contains("discardResult = { it.close() }"));
}
