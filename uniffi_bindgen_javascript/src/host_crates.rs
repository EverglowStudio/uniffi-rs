/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Optional Rust host-crate emitter for the JavaScript target.
//!
//! Downstream Node/Electron consumers need a `cdylib` Rust crate that
//! `#[napi]`-exports the generated `node/<crate>.rs`; browsers need a
//! `cdylib` that `#[wasm_bindgen]`-exports `browser/<crate>.rs`. Before
//! this module existed, each downstream project had to hand-maintain
//! `crates/<name>-wasm` and `crates/<name>-napi` shim crates. This
//! module generates those shim crates from the downstream `Cargo.toml`
//! so the only thing users have to keep around is their core crate.
//!
//! The feature is fully opt-in — invoking
//! `uniffi-bindgen generate --language javascript` without
//! `--emit-host-crates` produces the exact same tree as before.
//!
//! Electron does **not** get its own Rust crate; the `napi` host crate
//! is reused by the Electron consumption form (see
//! `electron/mod.rs`).

use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::MetadataCommand;
use fs_err as fs;
use sha2::{Digest, Sha256};

/// Caller-supplied metadata + CLI flags for host-crate emission.
#[derive(Clone, Debug)]
pub struct HostCrateOptions {
    /// Path to the downstream core crate's `Cargo.toml`.
    pub manifest_path: Utf8PathBuf,
    /// Directory (usually `rust_modules`) in which to write
    /// `wasm/`, `napi/`, and/or `ohos/` subcrates. Resolved relative to
    /// the current working directory if not absolute.
    pub host_crates_dir: Utf8PathBuf,
    /// Logical publication directory used when generated Cargo manifests
    /// compute path dependencies. Invocation-private artifact builds write to
    /// `host_crates_dir` but must retain paths valid after the tree is moved to
    /// this final location.
    pub logical_host_crates_dir: Option<Utf8PathBuf>,
    /// Logical publication root for generated bridge files. Invocation-private
    /// coordinators use this to emit `include!` paths that remain valid after
    /// the host crate and bindings are published.
    pub logical_out_dir: Option<Utf8PathBuf>,
    /// Optional local checkout of `ohos-rs`; when set, the OHOS host crate
    /// uses path dependencies instead of crates.io versions.
    pub ohos_rs_dir: Option<Utf8PathBuf>,
}

/// Manifest metadata extracted from the downstream `Cargo.toml`.
#[derive(Clone, Debug)]
pub struct CoreCrateMetadata {
    pub package_name: String,
    pub package_version: String,
    pub description: Option<String>,
    pub authors: Vec<String>,
    pub license: Option<String>,
    pub crate_dir: Utf8PathBuf,
    pub uniffi_dep: Option<UniffiDependency>,
}

#[derive(Clone, Debug)]
pub struct UniffiDependency {
    pub base_dir: Utf8PathBuf,
    pub version: Option<String>,
    pub path: Option<String>,
    pub git: Option<String>,
    pub branch: Option<String>,
    pub tag: Option<String>,
    pub rev: Option<String>,
    pub package: Option<String>,
    pub default_features: Option<bool>,
    pub features: Vec<String>,
}

pub fn load_metadata(manifest_path: &Utf8Path) -> Result<CoreCrateMetadata> {
    let text = fs::read_to_string(manifest_path)
        .with_context(|| format!("reading manifest at {manifest_path}"))?;
    let value: toml::Value =
        toml::from_str(&text).with_context(|| format!("parsing {manifest_path}"))?;
    // Cargo owns workspace inheritance semantics.  Reading the package table
    // directly would see `version.workspace = true` as a table and would also
    // silently lose inherited optional metadata.  Use Cargo's resolved model
    // for every field that can be inherited, while retaining the source TOML
    // below only for rendering the original `uniffi` dependency declaration.
    let cargo_metadata = MetadataCommand::new()
        .manifest_path(manifest_path.as_std_path())
        .no_deps()
        .exec()
        .with_context(|| format!("running cargo metadata for {manifest_path}"))?;
    let canonical_manifest = manifest_path
        .canonicalize_utf8()
        .unwrap_or_else(|_| manifest_path.to_path_buf());
    let package = cargo_metadata
        .packages
        .iter()
        .find(|package| {
            let package_manifest =
                Utf8PathBuf::from_path_buf(package.manifest_path.clone().into_std_path_buf()).ok();
            package_manifest
                .map(|path| path.canonicalize_utf8().unwrap_or(path) == canonical_manifest)
                .unwrap_or(false)
        })
        .or_else(|| cargo_metadata.root_package())
        .with_context(|| format!("cargo metadata did not resolve package {manifest_path}"))?;
    let package_name = package.name.to_string();
    let package_version = package.version.to_string();
    let description = package.description.clone();
    let authors = package.authors.clone();
    let license = package.license.clone();
    let uniffi_dep = resolve_uniffi_dependency(manifest_path, &value)?;
    let crate_dir = manifest_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| Utf8PathBuf::from("."));
    let crate_dir = crate_dir.canonicalize_utf8().unwrap_or_else(|_| crate_dir);
    Ok(CoreCrateMetadata {
        package_name,
        package_version,
        description,
        authors,
        license,
        crate_dir,
        uniffi_dep,
    })
}

/// Emit `<host_crates_dir>/wasm/*` and `<host_crates_dir>/napi/*`.
///
/// `out_dir` is the JS target's `--out-dir` (already populated with
/// `browser/<crate>.rs` and `node/<crate>.rs` by earlier steps).
/// `crate_names` is the list of uniffi component crate names for which
/// `.rs` bridge files were written — both host crates `include!` one
/// module per crate.
pub fn emit(
    options: &HostCrateOptions,
    out_dir: &Utf8Path,
    crate_names: &[String],
    meta: &CoreCrateMetadata,
    want_wasm: bool,
    want_napi: bool,
    want_ohos: bool,
    namespaces: &[String],
) -> Result<()> {
    if crate_names.is_empty() {
        bail!("host-crate emission requested but no components were generated");
    }
    if !want_wasm && !want_napi && !want_ohos {
        return Ok(());
    }
    let host_dir = if options.host_crates_dir.is_absolute() {
        options.host_crates_dir.clone()
    } else {
        let cwd = Utf8PathBuf::from_path_buf(std::env::current_dir()?)
            .map_err(|p| anyhow::anyhow!("cwd is not utf8: {}", p.display()))?;
        cwd.join(&options.host_crates_dir)
    };
    fs::create_dir_all(&host_dir)?;

    let out_dir_abs = out_dir
        .canonicalize_utf8()
        .with_context(|| format!("canonicalizing out_dir {out_dir}"))?;
    let host_dir_abs = host_dir
        .canonicalize_utf8()
        .with_context(|| format!("canonicalizing {host_dir}"))?;
    let logical_host_dir = options
        .logical_host_crates_dir
        .clone()
        .unwrap_or_else(|| host_dir_abs.clone());
    let logical_host_dir = if logical_host_dir.is_absolute() {
        logical_host_dir
    } else {
        Utf8PathBuf::from_path_buf(std::env::current_dir()?)
            .map_err(|p| anyhow::anyhow!("cwd is not utf8: {}", p.display()))?
            .join(logical_host_dir)
    };
    let logical_out_dir = options
        .logical_out_dir
        .clone()
        .unwrap_or_else(|| out_dir_abs.clone());

    if want_wasm {
        emit_wasm(
            &host_dir_abs,
            &logical_host_dir,
            &logical_out_dir,
            crate_names,
            meta,
        )?;
    }
    if want_napi {
        emit_napi(
            &host_dir_abs,
            &logical_host_dir,
            &out_dir_abs,
            &logical_out_dir,
            crate_names,
            meta,
        )?;
    }
    if want_ohos {
        emit_ohos(
            &host_dir_abs,
            &logical_host_dir,
            &out_dir_abs,
            &logical_out_dir,
            crate_names,
            meta,
            options.ohos_rs_dir.as_ref(),
            namespaces,
        )?;
    }
    Ok(())
}

fn emit_wasm(
    host_dir: &Utf8Path,
    logical_host_dir: &Utf8Path,
    out_dir: &Utf8Path,
    crate_names: &[String],
    meta: &CoreCrateMetadata,
) -> Result<()> {
    let crate_dir = host_dir.join("wasm");
    let logical_crate_dir = logical_host_dir.join("wasm");
    let src_dir = crate_dir.join("src");
    let logical_src_dir = logical_crate_dir.join("src");
    fs::create_dir_all(&src_dir)?;

    let rel_core = relative_path(&logical_crate_dir, &meta.crate_dir);
    let package_name = format!("{}-wasm", meta.package_name);

    let cargo_toml = format!(
        "# AUTOGENERATED by uniffi_bindgen_javascript (host crate: wasm).\n\
         # Regenerate via `uniffi-bindgen generate --language javascript \\\n\
         #   --flavor wasm --emit-host-crates --manifest-path <core Cargo.toml>`.\n\
         [package]\n\
         name = \"{package_name}\"\n\
         version = \"0.0.0\"\n\
         edition = \"2021\"\n\
         publish = false\n\
         \n\
         [lib]\n\
         crate-type = [\"cdylib\", \"rlib\"]\n\
         \n\
         [dependencies]\n\
         {core_name} = {{ path = \"{rel_core}\" }}\n\
         {uniffi_dep}\
         async-trait = \"0.1\"\n\
         wasm-bindgen = \"=0.2.117\"\n\
         wasm-bindgen-futures = \"0.4\"\n\
         js-sys = \"0.3\"\n\
         \n\
         [profile.release]\n\
         opt-level = \"z\"\n\
         lto = \"fat\"\n\
         codegen-units = 1\n\
         panic = \"abort\"\n\
         strip = true\n\
         debug = false\n\
         \n\
         [workspace]\n\
         resolver = \"3\"\n",
        package_name = package_name,
        core_name = meta.package_name,
        rel_core = rel_core,
        uniffi_dep = render_uniffi_dependency(meta.uniffi_dep.as_ref(), &logical_crate_dir)?,
    );
    fs::write(crate_dir.join("Cargo.toml"), cargo_toml)?;

    let mut lib_rs = String::from(
        "// AUTOGENERATED by uniffi_bindgen_javascript (host crate: wasm).\n\
         //\n\
         // Each `include!` below pastes the generator's per-component\n\
         // wasm-bindgen shim into this crate, so `cargo build --target\n\
         // wasm32-unknown-unknown` produces the final `cdylib`.\n\n",
    );
    for crate_name in crate_names {
        let rs_path = out_dir.join("browser").join(format!("{crate_name}.rs"));
        let rel = relative_path(&logical_src_dir, &rs_path);
        lib_rs.push_str(&format!("include!(\"{rel}\");\n"));
    }
    fs::write(src_dir.join("lib.rs"), lib_rs)?;
    Ok(())
}

fn emit_napi(
    host_dir: &Utf8Path,
    logical_host_dir: &Utf8Path,
    actual_out_dir: &Utf8Path,
    logical_out_dir: &Utf8Path,
    crate_names: &[String],
    meta: &CoreCrateMetadata,
) -> Result<()> {
    let crate_dir = host_dir.join("napi");
    let logical_crate_dir = logical_host_dir.join("napi");
    let src_dir = crate_dir.join("src");
    let logical_src_dir = logical_crate_dir.join("src");
    fs::create_dir_all(&src_dir)?;

    let rel_core = relative_path(&logical_crate_dir, &meta.crate_dir);
    let package_name = format!("{}-napi", meta.package_name);

    let cargo_toml = format!(
        "# AUTOGENERATED by uniffi_bindgen_javascript (host crate: napi).\n\
         # Regenerate via `uniffi-bindgen generate --language javascript \\\n\
         #   --flavor napi --emit-host-crates --manifest-path <core Cargo.toml>`.\n\
         # Also reused by the electron consumption form — electron does\n\
         # NOT get its own Rust host crate.\n\
         [package]\n\
         name = \"{package_name}\"\n\
         version = \"0.0.0\"\n\
         edition = \"2021\"\n\
         publish = false\n\
         \n\
         [lib]\n\
         crate-type = [\"cdylib\"]\n\
         \n\
         [dependencies]\n\
         {core_name} = {{ path = \"{rel_core}\" }}\n\
         {uniffi_dep}\
         async-trait = \"0.1\"\n\
         napi = {{ version = \"3.8.4\", default-features = false, features = [\"napi8\", \"tokio_rt\"] }}\n\
         napi-derive = {{ version = \"3.5.3\", features = [\"type-def\"] }}\n\
         \n\
         [build-dependencies]\n\
         napi-build = \"2.3.1\"\n\
         \n\
         [workspace]\n\
         resolver = \"3\"\n",
        package_name = package_name,
        core_name = meta.package_name,
        rel_core = rel_core,
        uniffi_dep = render_uniffi_dependency(meta.uniffi_dep.as_ref(), &logical_crate_dir)?,
    );
    fs::write(crate_dir.join("Cargo.toml"), cargo_toml)?;

    let build_rs = "// AUTOGENERATED by uniffi_bindgen_javascript (host crate: napi).\n\
                    extern crate napi_build;\n\
                    fn main() {\n    napi_build::setup();\n}\n";
    fs::write(crate_dir.join("build.rs"), build_rs)?;

    let mut lib_rs = String::from(
        "// AUTOGENERATED by uniffi_bindgen_javascript (host crate: napi).\n\
         //\n\
         // Each `include!` below pastes the generator's per-component\n\
         // napi-rs bridge into this crate, so `cargo build` produces the\n\
         // final `.node` cdylib consumed by the generated `backend-napi.ts`.\n\n",
    );
    for crate_name in crate_names {
        let actual_node_rs_path = actual_out_dir.join("node").join(format!("{crate_name}.rs"));
        let flavor = if actual_node_rs_path.exists() {
            "node"
        } else {
            "electron"
        };
        let rs_path = logical_out_dir
            .join(flavor)
            .join(format!("{crate_name}.rs"));
        let rel = relative_path(&logical_src_dir, &rs_path);
        lib_rs.push_str(&format!("include!(\"{rel}\");\n"));
    }
    fs::write(src_dir.join("lib.rs"), lib_rs)?;
    Ok(())
}

fn emit_ohos(
    host_dir: &Utf8Path,
    logical_host_dir: &Utf8Path,
    actual_out_dir: &Utf8Path,
    logical_out_dir: &Utf8Path,
    crate_names: &[String],
    meta: &CoreCrateMetadata,
    ohos_rs_dir: Option<&Utf8PathBuf>,
    namespaces: &[String],
) -> Result<()> {
    let crate_dir = host_dir.join("ohos");
    let logical_crate_dir = logical_host_dir.join("ohos");
    let src_dir = crate_dir.join("src");
    let logical_src_dir = logical_crate_dir.join("src");
    fs::create_dir_all(&src_dir)?;

    let rel_core = relative_path(&logical_crate_dir, &meta.crate_dir);
    let package_name = format!("{}-ohos", meta.package_name);
    let ohos_deps = render_ohos_dependencies(ohos_rs_dir, &logical_crate_dir)?;
    let package_metadata = render_ohos_package_metadata(meta);
    let lib_name = match namespaces {
        [namespace] => crate::js_names::ohos_native_library_stem(namespace),
        [] => bail!("OHOS host-crate emission requested but no component namespace was generated"),
        _ => bail!(
            "OHOS host-crate emission currently supports one component per host crate; got namespaces: {namespaces:?}"
        ),
    };

    let cargo_toml = format!(
        "# AUTOGENERATED by uniffi_bindgen_javascript (host crate: ohos).\n\
         # Regenerate via `uniffi-bindgen javascript build-ohos \\\n\
         #   --manifest-path <core Cargo.toml> --out-dir <generated>`.\n\
         [package]\n\
         name = \"{package_name}\"\n\
         {package_metadata}\
         edition = \"2021\"\n\
         publish = false\n\
         \n\
         [lib]\n\
         name = \"{lib_name}\"\n\
         crate-type = [\"cdylib\"]\n\
         \n\
         [dependencies]\n\
         {core_name} = {{ path = \"{rel_core}\" }}\n\
         {uniffi_dep}\
         async-trait = \"0.1\"\n\
         {ohos_deps}\
         \n\
         [workspace]\n\
         resolver = \"3\"\n",
        package_name = package_name,
        package_metadata = package_metadata,
        lib_name = lib_name,
        core_name = meta.package_name,
        rel_core = rel_core,
        uniffi_dep = render_uniffi_dependency(meta.uniffi_dep.as_ref(), &logical_crate_dir)?,
        ohos_deps = ohos_deps,
    );
    fs::write(crate_dir.join("Cargo.toml"), cargo_toml)?;

    // The facade contract is an invocation input, not a Cargo build-script
    // output.  Keeping the complete, checksummed bundle beside the generated
    // host manifest lets the OHOS packager install it on every invocation,
    // including when Cargo correctly decides that this host crate is fresh.
    let mut contracts = Vec::new();
    let mut type_sidecars = Vec::new();
    let mut components = Vec::new();
    for crate_name in crate_names {
        let contract_file = format!("{crate_name}.ohos-facade.json");
        let contract_path = actual_out_dir.join("harmony").join(&contract_file);
        let contract_content = fs::read_to_string(&contract_path)
            .with_context(|| format!("reading generated OHOS facade contract {contract_path}"))?;
        let contract_digest = sha256_text(&contract_content);
        contracts.push(serde_json::json!({
            "file": contract_file,
            "sha256": contract_digest,
            "content": contract_content,
        }));
        components.push(serde_json::json!({
            "component": crate_name,
            "contractFile": format!("{crate_name}.ohos-facade.json"),
            "contractSha256": contract_digest,
            "identityExport": crate::flavors::napi::ohos_bridge_identity_export(&contract_digest),
        }));

        let sidecar_file = format!("{crate_name}.ohos-extra-types.d.ts");
        let sidecar_path = actual_out_dir.join("harmony").join(&sidecar_file);
        let sidecar_content = fs::read_to_string(&sidecar_path)
            .with_context(|| format!("reading generated OHOS type sidecar {sidecar_path}"))?;
        type_sidecars.push(serde_json::json!({
            "file": sidecar_file,
            "sha256": sha256_text(&sidecar_content),
            "content": sidecar_content,
        }));
    }
    contracts.sort_by(|left, right| left["file"].as_str().cmp(&right["file"].as_str()));
    type_sidecars.sort_by(|left, right| left["file"].as_str().cmp(&right["file"].as_str()));
    components.sort_by(|left, right| left["component"].as_str().cmp(&right["component"].as_str()));
    let host_identity_payload = serde_json::json!({
        "packageName": package_name.clone(),
        "libTarget": lib_name.clone(),
        "components": components.clone(),
    });
    let host_composite_identity = sha256_bytes(&serde_json::to_vec(&host_identity_payload)?);
    let payload = serde_json::json!({
        "packageName": package_name,
        "libTarget": lib_name,
        "hostCompositeIdentity": host_composite_identity,
        "components": host_identity_payload["components"],
        "contracts": contracts,
        "typeSidecars": type_sidecars,
    });
    let payload_bytes = serde_json::to_vec(&payload)?;
    let bundle = serde_json::json!({
        "schemaVersion": 2,
        "fingerprint": sha256_bytes(&payload_bytes),
        "packageName": payload["packageName"],
        "libTarget": payload["libTarget"],
        "hostCompositeIdentity": payload["hostCompositeIdentity"],
        "components": payload["components"],
        "contracts": payload["contracts"],
        "typeSidecars": payload["typeSidecars"],
    });
    let mut bundle_text = serde_json::to_string_pretty(&bundle)?;
    bundle_text.push('\n');
    fs::write(
        crate_dir.join("uniffi-ohos-facade-bundle.json"),
        bundle_text,
    )?;

    let build_rs = r#"// AUTOGENERATED by uniffi_bindgen_javascript (host crate: ohos).
extern crate napi_build_ohos;
fn main() {
    napi_build_ohos::setup();
    println!("cargo:rustc-link-arg=-Wl,--wrap=napi_add_env_cleanup_hook");
    println!("cargo:rustc-link-arg=-Wl,--wrap=napi_remove_env_cleanup_hook");
}
"#;
    fs::write(crate_dir.join("build.rs"), build_rs)?;

    let mut lib_rs = String::from(
        r#"// AUTOGENERATED by uniffi_bindgen_javascript (host crate: ohos).
//
// Each `include!` below pastes the generator's per-component
// ohos-rs bridge into this crate, so `ohrs build` produces the
// final Harmony/OpenHarmony `lib*.so` cdylib.

// HarmonyOS indexes environment cleanup hooks by the `arg` pointer alone.
// napi-ohos uses a null key for its Tokio runtime hook, which collides with
// another native module doing the same. The linker wrappers substitute stable
// callback-specific keys owned by this cdylib and preserve them for removal.
mod __uniffi_napi_cleanup_hook_key {
    use napi_ohos::sys::{napi_env, napi_status};
    use std::collections::BTreeMap;
    use std::ffi::c_void;
    use std::sync::{Mutex, OnceLock};

    type CleanupHook = Option<unsafe extern "C" fn(*mut c_void)>;

    // The boxes never move or get removed, so their addresses are stable for
    // the lifetime of this cdylib.  Keying the map by the callback pointer
    // gives different null-arg callbacks different keys while the per-cdylib
    // static keeps otherwise identical callbacks in different DSOs distinct.
    static CLEANUP_HOOK_KEYS: OnceLock<Mutex<BTreeMap<usize, Box<u8>>>> = OnceLock::new();

    // `--wrap` rewrites calls to these definitions. Mark only the two wrapper
    // symbols protected so the dynamic linker cannot interpose a main-program
    // or earlier-DSO definition on calls originating in this cdylib.
    core::arch::global_asm!(
        ".protected __wrap_napi_add_env_cleanup_hook",
        ".protected __wrap_napi_remove_env_cleanup_hook",
    );

    unsafe extern "C" {
        fn __real_napi_add_env_cleanup_hook(
            env: napi_env,
            fun: CleanupHook,
            arg: *mut c_void,
        ) -> napi_status;
        fn __real_napi_remove_env_cleanup_hook(
            env: napi_env,
            fun: CleanupHook,
            arg: *mut c_void,
        ) -> napi_status;
    }

    fn unique_arg(fun: CleanupHook, arg: *mut c_void) -> *mut c_void {
        if !arg.is_null() {
            return arg;
        }

        let callback = fun.map_or(0, |callback| callback as *const () as usize);
        let keys = CLEANUP_HOOK_KEYS.get_or_init(|| Mutex::new(BTreeMap::new()));
        let mut keys = keys.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = keys.entry(callback).or_insert_with(|| Box::new(0));
        core::ptr::from_mut(key.as_mut()).cast()
    }

    #[no_mangle]
    unsafe extern "C" fn __wrap_napi_add_env_cleanup_hook(
        env: napi_env,
        fun: CleanupHook,
        arg: *mut c_void,
    ) -> napi_status {
        unsafe { __real_napi_add_env_cleanup_hook(env, fun, unique_arg(fun, arg)) }
    }

    #[no_mangle]
    unsafe extern "C" fn __wrap_napi_remove_env_cleanup_hook(
        env: napi_env,
        fun: CleanupHook,
        arg: *mut c_void,
    ) -> napi_status {
        unsafe { __real_napi_remove_env_cleanup_hook(env, fun, unique_arg(fun, arg)) }
    }
}

"#,
    );
    for crate_name in crate_names {
        let rs_path = logical_out_dir
            .join("harmony")
            .join(format!("{crate_name}.rs"));
        let rel = relative_path(&logical_src_dir, &rs_path);
        lib_rs.push_str(&format!("include!(\"{rel}\");\n"));
    }
    fs::write(src_dir.join("lib.rs"), lib_rs)?;
    Ok(())
}

fn sha256_text(value: &str) -> String {
    sha256_bytes(value.as_bytes())
}

fn sha256_bytes(value: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(value);
    format!("{:x}", digest.finalize())
}

fn render_ohos_package_metadata(meta: &CoreCrateMetadata) -> String {
    let mut out = format!("version = {}\n", toml_string_literal(&meta.package_version));
    if let Some(description) = &meta.description {
        out.push_str(&format!(
            "description = {}\n",
            toml_string_literal(description)
        ));
    }
    if !meta.authors.is_empty() {
        let authors = meta
            .authors
            .iter()
            .map(|author| toml_string_literal(author))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("authors = [{authors}]\n"));
    }
    if let Some(license) = &meta.license {
        out.push_str(&format!("license = {}\n", toml_string_literal(license)));
    }
    out
}

fn toml_string_literal(value: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => {
                let _ = write!(out, "\\u{:04X}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('\"');
    out
}

fn render_ohos_dependencies(
    ohos_rs_dir: Option<&Utf8PathBuf>,
    crate_dir: &Utf8Path,
) -> Result<String> {
    if let Some(ohos_rs_dir) = ohos_rs_dir {
        let abs = ohos_rs_dir
            .canonicalize_utf8()
            .unwrap_or_else(|_| ohos_rs_dir.clone());
        let napi = relative_path(crate_dir, &abs.join("crates/napi"));
        let derive = relative_path(crate_dir, &abs.join("crates/macro"));
        let build = relative_path(crate_dir, &abs.join("crates/build"));
        return Ok(format!(
            "napi-ohos = {{ path = \"{napi}\", default-features = false, features = [\"napi8\", \"tokio_rt\"] }}\n\
             napi-derive-ohos = {{ path = \"{derive}\", features = [\"type-def\"] }}\n\
             \n\
             [build-dependencies]\n\
             napi-build-ohos = {{ path = \"{build}\" }}\n"
        ));
    }
    Ok(
        "napi-ohos = { version = \"1.1.6\", default-features = false, features = [\"napi8\", \"tokio_rt\"] }\n\
         napi-derive-ohos = { version = \"1.1.6\", features = [\"type-def\"] }\n\
         \n\
         [build-dependencies]\n\
         napi-build-ohos = \"1.1.6\"\n"
            .to_string(),
    )
}

/// Compute a relative path from `from_dir` (a directory) to `to` (any
/// path). Both inputs should already be canonicalized (absolute) so the
/// result has no surprises from `.` / `..` components in the original.
fn relative_path(from_dir: &Utf8Path, to: &Utf8Path) -> Utf8PathBuf {
    let from: Vec<&str> = from_dir.components().map(|c| c.as_str()).collect();
    let to_vec: Vec<&str> = to.components().map(|c| c.as_str()).collect();
    let mut i = 0;
    while i < from.len() && i < to_vec.len() && from[i] == to_vec[i] {
        i += 1;
    }
    let mut result = Utf8PathBuf::new();
    for _ in i..from.len() {
        result.push("..");
    }
    for c in &to_vec[i..] {
        result.push(c);
    }
    if result.as_str().is_empty() {
        result.push(".");
    }
    result
}

fn resolve_uniffi_dependency(
    manifest_path: &Utf8Path,
    value: &toml::Value,
) -> Result<Option<UniffiDependency>> {
    let manifest_dir = manifest_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| Utf8PathBuf::from("."));
    let Some(dep) = value
        .get("dependencies")
        .and_then(|deps| deps.get("uniffi"))
    else {
        return Ok(None);
    };
    if dep
        .get("workspace")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return resolve_workspace_uniffi_dependency(&manifest_dir).map(Some);
    }
    parse_uniffi_dependency(dep, &manifest_dir).map(Some)
}

fn resolve_workspace_uniffi_dependency(start_dir: &Utf8Path) -> Result<UniffiDependency> {
    let mut current = Some(start_dir);
    while let Some(dir) = current {
        let manifest = dir.join("Cargo.toml");
        if manifest.exists() {
            let text = fs::read_to_string(&manifest)
                .with_context(|| format!("reading workspace manifest at {manifest}"))?;
            let value: toml::Value =
                toml::from_str(&text).with_context(|| format!("parsing {manifest}"))?;
            if let Some(dep) = value
                .get("workspace")
                .and_then(|ws| ws.get("dependencies"))
                .and_then(|deps| deps.get("uniffi"))
            {
                return parse_uniffi_dependency(dep, dir);
            }
        }
        current = dir.parent();
    }
    bail!("could not resolve workspace dependency for `uniffi` starting from {start_dir}")
}

fn parse_uniffi_dependency(dep: &toml::Value, base_dir: &Utf8Path) -> Result<UniffiDependency> {
    match dep {
        toml::Value::String(version) => Ok(UniffiDependency {
            base_dir: base_dir.to_path_buf(),
            version: Some(version.clone()),
            path: None,
            git: None,
            branch: None,
            tag: None,
            rev: None,
            package: None,
            default_features: None,
            features: Vec::new(),
        }),
        toml::Value::Table(table) => Ok(UniffiDependency {
            base_dir: base_dir.to_path_buf(),
            version: table
                .get("version")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned),
            path: table
                .get("path")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned),
            git: table
                .get("git")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned),
            branch: table
                .get("branch")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned),
            tag: table
                .get("tag")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned),
            rev: table
                .get("rev")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned),
            package: table
                .get("package")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned),
            default_features: table.get("default-features").and_then(|v| v.as_bool()),
            features: table
                .get("features")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        }),
        _ => bail!("unsupported `uniffi` dependency form in Cargo.toml"),
    }
}

fn render_uniffi_dependency(
    dep: Option<&UniffiDependency>,
    crate_dir: &Utf8Path,
) -> Result<String> {
    let Some(dep) = dep else {
        return Ok(String::new());
    };
    let mut fields: Vec<(String, String)> = Vec::new();
    if let Some(path) = &dep.path {
        let abs = dep
            .base_dir
            .join(path)
            .canonicalize_utf8()
            .unwrap_or_else(|_| dep.base_dir.join(path));
        let rel = relative_path(crate_dir, &abs);
        fields.push(("path".into(), format!("\"{rel}\"")));
    }
    if let Some(version) = &dep.version {
        fields.push(("version".into(), format!("\"{version}\"")));
    }
    if let Some(git) = &dep.git {
        fields.push(("git".into(), format!("\"{git}\"")));
    }
    if let Some(branch) = &dep.branch {
        fields.push(("branch".into(), format!("\"{branch}\"")));
    }
    if let Some(tag) = &dep.tag {
        fields.push(("tag".into(), format!("\"{tag}\"")));
    }
    if let Some(rev) = &dep.rev {
        fields.push(("rev".into(), format!("\"{rev}\"")));
    }
    if let Some(package) = &dep.package {
        fields.push(("package".into(), format!("\"{package}\"")));
    }
    if let Some(default_features) = dep.default_features {
        fields.push(("default-features".into(), default_features.to_string()));
    }
    if !dep.features.is_empty() {
        let features = dep
            .features
            .iter()
            .map(|feature| format!("\"{feature}\""))
            .collect::<Vec<_>>()
            .join(", ");
        fields.push(("features".into(), format!("[{features}]")));
    }
    if fields.is_empty() {
        bail!("resolved `uniffi` dependency has no renderable fields");
    }
    let fields = fields
        .into_iter()
        .map(|(key, value)| format!("{key} = {value}"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!("uniffi = {{ {fields} }}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "uniffi-js-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn write_minimal_lib(crate_dir: &std::path::Path) {
        std::fs::create_dir_all(crate_dir.join("src")).unwrap();
        std::fs::write(crate_dir.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
    }

    #[test]
    fn carries_core_cargo_metadata_into_ohos_host_manifest() {
        let root = test_root("host-meta");
        std::fs::create_dir_all(&root).unwrap();
        write_minimal_lib(&root);
        let manifest = root.join("Cargo.toml");
        std::fs::write(
            &manifest,
            r#"[package]
name = "demo-core"
version = "1.2.3"
description = "quoted \"metadata\""
authors = ["First Author <first@example.com>", "Second Author"]
license = "MPL-2.0"
edition = "2021"
"#,
        )
        .unwrap();
        let manifest = Utf8PathBuf::from_path_buf(manifest).unwrap();
        let metadata = load_metadata(&manifest).unwrap();
        assert_eq!(metadata.package_name, "demo-core");
        assert_eq!(metadata.package_version, "1.2.3");
        assert_eq!(metadata.description.as_deref(), Some("quoted \"metadata\""));
        assert_eq!(
            metadata.authors,
            vec!["First Author <first@example.com>", "Second Author"]
        );
        assert_eq!(metadata.license.as_deref(), Some("MPL-2.0"));

        let rendered = format!(
            "[package]\nname = \"demo-core-ohos\"\n{}edition = \"2021\"\n",
            render_ohos_package_metadata(&metadata)
        );
        let parsed: toml::Value = toml::from_str(&rendered).unwrap();
        assert_eq!(parsed["package"]["version"].as_str(), Some("1.2.3"));
        assert_eq!(
            parsed["package"]["description"].as_str(),
            Some("quoted \"metadata\"")
        );
        assert_eq!(parsed["package"]["authors"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["package"]["license"].as_str(), Some("MPL-2.0"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn resolves_workspace_inherited_metadata_into_real_ohos_host_manifest() {
        let root = test_root("host-workspace-meta");
        let core = root.join("core");
        let out = root.join("generated");
        let host = root.join("host");
        write_minimal_lib(&core);
        std::fs::create_dir_all(out.join("harmony")).unwrap();
        std::fs::write(out.join("harmony/demo_core.rs"), "").unwrap();
        for (name, body) in [
            ("native-facade.ets", "export default {};\n"),
            ("package-index.ets", "export default {};\n"),
            ("demo_core.ohos-extra-types.d.ts", "export {};\n"),
            (
                "demo_core.ohos-facade.json",
                "{\"schemaVersion\":2,\"component\":\"demo_core\",\"outputStreams\":[],\"inputStreams\":[]}",
            ),
        ] {
            std::fs::write(out.join("harmony").join(name), body).unwrap();
        }
        std::fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["core"]
resolver = "2"

[workspace.package]
version = "4.5.6"
description = "inherited description"
authors = ["Workspace Author <author@example.com>"]
license = "Apache-2.0"
edition = "2021"
"#,
        )
        .unwrap();
        std::fs::write(
            core.join("Cargo.toml"),
            r#"[package]
name = "demo-core"
version.workspace = true
description.workspace = true
authors.workspace = true
license.workspace = true
edition.workspace = true
"#,
        )
        .unwrap();

        let manifest = Utf8PathBuf::from_path_buf(core.join("Cargo.toml")).unwrap();
        let metadata = load_metadata(&manifest).unwrap();
        assert_eq!(metadata.package_version, "4.5.6");
        assert_eq!(
            metadata.description.as_deref(),
            Some("inherited description")
        );
        assert_eq!(metadata.authors, ["Workspace Author <author@example.com>"]);
        assert_eq!(metadata.license.as_deref(), Some("Apache-2.0"));

        let host = Utf8PathBuf::from_path_buf(host).unwrap();
        let out = Utf8PathBuf::from_path_buf(out).unwrap();
        let logical_host = Utf8PathBuf::from_path_buf(root.join("published/host")).unwrap();
        let logical_out = Utf8PathBuf::from_path_buf(root.join("published/generated")).unwrap();
        std::fs::create_dir_all(&host).unwrap();
        emit_ohos(
            &host,
            &logical_host,
            &out,
            &logical_out,
            &["demo_core".to_string()],
            &metadata,
            None,
            &["demo_core".to_string()],
        )
        .unwrap();
        let generated: toml::Value =
            toml::from_str(&std::fs::read_to_string(host.join("ohos/Cargo.toml")).unwrap())
                .unwrap();
        let build_rs = std::fs::read_to_string(host.join("ohos/build.rs")).unwrap();
        let lib_rs = std::fs::read_to_string(host.join("ohos/src/lib.rs")).unwrap();
        assert!(!build_rs.contains("demo_core.ohos-facade.json"));
        assert!(!build_rs.contains("uniffi-ohos-facade-inventory.json"));
        assert!(!build_rs.contains("let _ = std::fs::write"));
        assert!(build_rs.contains("--wrap=napi_add_env_cleanup_hook"));
        assert!(build_rs.contains("--wrap=napi_remove_env_cleanup_hook"));
        assert!(
            lib_rs.contains("static CLEANUP_HOOK_KEYS: OnceLock<Mutex<BTreeMap<usize, Box<u8>>>>")
        );
        assert!(lib_rs.contains(".protected __wrap_napi_add_env_cleanup_hook"));
        assert!(lib_rs.contains(".protected __wrap_napi_remove_env_cleanup_hook"));
        assert!(lib_rs.contains("__wrap_napi_add_env_cleanup_hook"));
        assert!(lib_rs.contains("__wrap_napi_remove_env_cleanup_hook"));
        assert!(lib_rs.contains("unique_arg(fun, arg)"));
        assert!(
            lib_rs.contains("include!(\"../../../generated/harmony/demo_core.rs\");"),
            "logical publication paths were not used for include!: {lib_rs}"
        );
        let bundle_path = host.join("ohos/uniffi-ohos-facade-bundle.json");
        let bundle: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&bundle_path).unwrap()).unwrap();
        assert_eq!(bundle["schemaVersion"], 2);
        assert_eq!(bundle["packageName"], "demo-core-ohos");
        assert_eq!(bundle["libTarget"], "demo_core_ohos");
        assert_eq!(bundle["hostCompositeIdentity"].as_str().unwrap().len(), 64);
        assert_eq!(bundle["components"][0]["component"], "demo_core");
        assert!(bundle["components"][0]["identityExport"]
            .as_str()
            .unwrap()
            .starts_with("uniffiohosbridgeidentity"));
        assert_eq!(bundle["contracts"][0]["file"], "demo_core.ohos-facade.json");
        assert_eq!(
            bundle["typeSidecars"][0]["file"],
            "demo_core.ohos-extra-types.d.ts"
        );
        assert_eq!(bundle["fingerprint"].as_str().unwrap().len(), 64);
        assert_eq!(generated["package"]["version"].as_str(), Some("4.5.6"));
        assert_eq!(
            generated["package"]["description"].as_str(),
            Some("inherited description")
        );
        assert_eq!(generated["package"]["authors"].as_array().unwrap().len(), 1);
        assert_eq!(generated["package"]["license"].as_str(), Some("Apache-2.0"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn leaves_missing_optional_cargo_metadata_absent() {
        let root = test_root("host-minimal-meta");
        write_minimal_lib(&root);
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"minimal-core\"\nversion = \"0.7.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let manifest = Utf8PathBuf::from_path_buf(root.join("Cargo.toml")).unwrap();
        let metadata = load_metadata(&manifest).unwrap();
        assert_eq!(metadata.package_version, "0.7.0");
        assert!(metadata.description.is_none());
        assert!(metadata.authors.is_empty());
        assert!(metadata.license.is_none());
        let rendered = render_ohos_package_metadata(&metadata);
        assert!(!rendered.contains("description ="));
        assert!(!rendered.contains("authors ="));
        assert!(!rendered.contains("license ="));
        std::fs::remove_dir_all(root).ok();
    }
}
