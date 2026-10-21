//! Render the actual async template with typed scalar/nullable/container IR.
//! Include supported custom aliases whose nullability is not exposed by that IR.
use super::*;

#[test]
fn lifecycle_close_validates_before_language_renames() {
    let dir = std::env::temp_dir().join(format!("uniffi-jni-lifecycle-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"lifecycle_rename\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();
    let udl = Utf8PathBuf::from_path_buf(dir.join("src/lifecycle_rename.udl")).unwrap();
    std::fs::write(
        &udl,
        "namespace lifecycle_rename {}; interface Client { [Async] void close(); };",
    )
    .unwrap();
    let loader = uniffi_bindgen::BindgenLoader::new(
        uniffi_bindgen::BindgenPaths::default(),
        uniffi_bindgen::GlobalConfig::default(),
    );
    let metadata = loader.load_metadata(&udl).unwrap();
    let mut initial = loader.load_pipeline_initial_root(&udl, metadata).unwrap();
    let config = r#"
        [bindings.lifecycle]
        close_methods = { Client = "close" }
        [bindings.kotlin]
        cdylib_name = "lifecycle_rename"
        [bindings.kotlin.rename]
        Client = "RenamedClient"
        "Client.close" = "finish"
    "#;
    initial
        .namespaces
        .get_mut("lifecycle_rename")
        .unwrap()
        .config_toml = Some(config.into());
    let root = pipeline().execute(initial.clone()).unwrap();
    let rendered = BindingsPackage {
        package: root.packages[0].clone(),
    }
    .render()
    .unwrap();
    assert!(rendered.contains("override suspend fun `finish`()"));
    assert!(rendered.contains("uniffiCloseResult"));
    assert!(!rendered.contains("AutoCloseable"));
    initial
        .namespaces
        .get_mut("lifecycle_rename")
        .unwrap()
        .config_toml = Some(config.replace("Client = \"close\"", "Client = \"finish\""));
    assert!(pipeline().execute(initial).is_err());
}

struct CompletionRoot(Vec<CallableResult>);
impl CompletionRoot {
    fn rust_async_callable_results(&self) -> impl Iterator<Item = &CallableResult> {
        self.0.iter()
    }
}
#[derive(Template)]
#[template(syntax = "kt", escape = "none", path = "shared/Async.kt")]
struct CompletionTemplate<'a> {
    root: &'a CompletionRoot,
}

#[test]
fn completion_success_containers_render_typed_outputs() {
    let optional = |ty| Type::Optional {
        inner_type: Box::new(ty),
    };
    let boxed = |ty| Type::Box {
        inner_type: Box::new(ty),
    };
    let list = |ty| Type::Sequence {
        inner_type: Box::new(ty),
    };
    // FFI values are protocol inputs to controlled test lifts. Real generated
    // FFI signatures are independently checked by the existing native suites.
    let cases = [
        (801, Type::UInt32, "kotlin.UInt", FfiType::Int32),
        (802, optional(Type::UInt32), "kotlin.UInt?", FfiType::Int32),
        (
            803,
            boxed(optional(Type::UInt32)),
            "kotlin.UInt?",
            FfiType::Int32,
        ),
        (
            804,
            list(optional(Type::String)),
            "kotlin.collections.List<kotlin.String?>",
            FfiType::Int32,
        ),
        (
            805,
            optional(list(optional(Type::String))),
            "kotlin.collections.List<kotlin.String?>?",
            FfiType::Int32,
        ),
        (
            806,
            boxed(optional(list(optional(Type::String)))),
            "kotlin.collections.List<kotlin.String?>?",
            FfiType::Int32,
        ),
        (807, Type::String, "kotlin.String", FfiType::String),
    ];
    let mut results = vec![CallableResult {
        return_type: None,
        throws_type: None,
        return_ffi: ReturnFfi::Void,
    }];
    for (id, ty, type_kt, ffi) in cases {
        results.push(CallableResult {
            return_type: Some(TypeNode {
                id,
                ty,
                type_kt: type_kt.into(),
                type_rs: String::new(),
                has_from_unexpected_callback_error_impl: false,
                is_used_as_error: false,
                ffi_types: vec![ffi],
            }),
            throws_type: None,
            return_ffi: ReturnFfi::Void,
        });
    }
    let root = CompletionRoot(results);
    let rendered = CompletionTemplate { root: &root }.render().unwrap();
    assert_eq!(rendered.matches("class CompleteRustFuture").count(), 7);
    assert!(!rendered.contains("class CompleteRustFuture0"));
    assert!(rendered.contains("suspend fun awaitRustFuture0"));
    assert!(!rendered.contains("completedValue"));
    assert!(rendered.contains("fun getValue(): kotlin.collections.List<kotlin.String?>?"));
    for result in root.0.iter().filter(|r| r.return_type.is_some()) {
        let ty = result.return_type.as_ref().unwrap();
        let body = rendered
            .split(&format!("class {} {{", result.async_complete_class()))
            .nth(1)
            .unwrap()
            .split("\n}\n")
            .next()
            .unwrap();
        assert!(body.contains(&format!(
            "private var result: Result<{}>? = null",
            ty.type_kt
        )));
        assert!(body.contains(&format!("fun getValue(): {}", ty.type_kt)));
        assert!(body.contains("checkNotNull(result)"));
        assert!(body.contains(".getOrThrow()"));
    }
    // The S08 functional runner supplies an isolated output directory. The unit
    // still validates rendering without it, and never creates a shared target.
    if let Some(path) = std::env::var_os("S08_JNI_COMPLETION_RENDER_OUTPUT") {
        std::fs::write(path, rendered).unwrap();
    }
}

#[test]
fn nullable_custom_completion_uses_real_udl_and_config() {
    // Parse the supported UDL/config and run the production pipeline/templates.
    // A typealias can hide nullability from Type::Custom in that pipeline.
    let configured_output = std::env::var_os("S08_JNI_NULLABLE_CUSTOM_OUTPUT");
    let output = configured_output
        .clone()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("uniffi-jni-nullable-custom-{}", std::process::id()))
        });
    std::fs::create_dir_all(&output).unwrap();
    let output = Utf8PathBuf::from_path_buf(output).unwrap();
    std::fs::create_dir_all(output.join("src")).unwrap();
    std::fs::write(
        output.join("Cargo.toml"),
        "[package]\nname = \"nullable_custom\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();
    let udl = output.join("src/nullable_custom.udl");
    std::fs::write(
        &udl,
        "[Custom] typedef string CustomText;\nnamespace nullable_custom {\n    [Async] CustomText fetch_custom();\n};\n",
    ).unwrap();
    let config = r#"[bindings.kotlin]
package_name = "uniffi.nullable_custom"
cdylib_name = "nullable_custom"
[bindings.kotlin.custom_types.CustomText]
type_name = "kotlin.String?"
lift = "{}.takeIf { it.isNotEmpty() }"
lower = '{} ?: ""'
"#;
    std::fs::write(output.join("uniffi.toml"), config).unwrap();
    let loader = uniffi_bindgen::BindgenLoader::new(
        uniffi_bindgen::BindgenPaths::default(),
        uniffi_bindgen::GlobalConfig::default(),
    );
    let metadata = loader.load_metadata(&udl).unwrap();
    let mut initial = loader.load_pipeline_initial_root(&udl, metadata).unwrap();
    initial
        .namespaces
        .get_mut("nullable_custom")
        .unwrap()
        .config_toml = Some(config.into());
    let root = pipeline().execute(initial).unwrap();
    let result = root.rust_async_callable_results().next().unwrap();
    let ty = result.return_type.as_ref().unwrap();
    assert!(matches!(ty.ty, Type::Custom { .. }));
    assert_eq!(ty.type_kt, "uniffi.nullable_custom.`CustomText`");
    let shared = UniffiPackage {
        cdylib: root.cdylib_name().unwrap(),
        root: &root,
    }
    .render()
    .unwrap();
    let package = BindingsPackage {
        package: root.packages[0].clone(),
    }
    .render()
    .unwrap();
    assert!(package.contains("public typealias `CustomText` = kotlin.String?"));
    assert!(shared.contains("return builtinValue.takeIf { it.isNotEmpty() }"));
    assert!(shared.contains(&format!("fun getValue(): {}", ty.type_kt)));
    std::fs::write(output.join("Uniffi.kt"), shared).unwrap();
    std::fs::write(output.join("nullable_custom.kt"), package).unwrap();
    // Give the runtime gate a stable name using the actual pipeline result ID.
    std::fs::write(output.join("CompletionProbe.kt"), format!(
        "package uniffi\ntypealias TestCustomCompletion = {}\nfun testGeneratedCustomLift(value: String?): uniffi.nullable_custom.CustomText = {}(value)\n",
        result.async_complete_class(), ty.lift_fn_kt(),
    )).unwrap();
    if configured_output.is_none() {
        std::fs::remove_dir_all(output).unwrap();
    }
}
