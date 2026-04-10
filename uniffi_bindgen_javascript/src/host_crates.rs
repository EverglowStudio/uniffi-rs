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
use fs_err as fs;

/// Caller-supplied metadata + CLI flags for host-crate emission.
#[derive(Clone, Debug)]
pub struct HostCrateOptions {
    /// Path to the downstream core crate's `Cargo.toml`.
    pub manifest_path: Utf8PathBuf,
    /// Directory (usually `rust_modules`) in which to write
    /// `wasm/` and `napi/` subcrates. Resolved relative to the current
    /// working directory if not absolute.
    pub host_crates_dir: Utf8PathBuf,
}

/// Manifest metadata extracted from the downstream `Cargo.toml`.
#[derive(Clone, Debug)]
pub struct CoreCrateMetadata {
    pub package_name: String,
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
    let package_name = value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .with_context(|| format!("{manifest_path} missing [package].name"))?
        .to_string();
    let uniffi_dep = resolve_uniffi_dependency(manifest_path, &value)?;
    let crate_dir = manifest_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| Utf8PathBuf::from("."));
    let crate_dir = crate_dir.canonicalize_utf8().unwrap_or_else(|_| crate_dir);
    Ok(CoreCrateMetadata {
        package_name,
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
) -> Result<()> {
    if crate_names.is_empty() {
        bail!("host-crate emission requested but no components were generated");
    }
    if !want_wasm && !want_napi {
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

    if want_wasm {
        emit_wasm(&host_dir_abs, &out_dir_abs, crate_names, meta)?;
    }
    if want_napi {
        emit_napi(&host_dir_abs, &out_dir_abs, crate_names, meta)?;
    }
    Ok(())
}

fn emit_wasm(
    host_dir: &Utf8Path,
    out_dir: &Utf8Path,
    crate_names: &[String],
    meta: &CoreCrateMetadata,
) -> Result<()> {
    let crate_dir = host_dir.join("wasm");
    let src_dir = crate_dir.join("src");
    fs::create_dir_all(&src_dir)?;

    let rel_core = relative_path(&crate_dir, &meta.crate_dir);
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
         wasm-bindgen = \"=0.2.117\"\n\
         wasm-bindgen-futures = \"0.4\"\n\
         js-sys = \"0.3\"\n\
         \n\
         [profile.release]\n\
         opt-level = \"s\"\n\
         \n\
         [workspace]\n",
        package_name = package_name,
        core_name = meta.package_name,
        rel_core = rel_core,
        uniffi_dep = render_uniffi_dependency(meta.uniffi_dep.as_ref(), &crate_dir)?,
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
        let rel = relative_path(&src_dir, &rs_path);
        lib_rs.push_str(&format!("include!(\"{rel}\");\n"));
    }
    fs::write(src_dir.join("lib.rs"), lib_rs)?;
    Ok(())
}

fn emit_napi(
    host_dir: &Utf8Path,
    out_dir: &Utf8Path,
    crate_names: &[String],
    meta: &CoreCrateMetadata,
) -> Result<()> {
    let crate_dir = host_dir.join("napi");
    let src_dir = crate_dir.join("src");
    fs::create_dir_all(&src_dir)?;

    let rel_core = relative_path(&crate_dir, &meta.crate_dir);
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
         napi = {{ version = \"3.8.4\", default-features = false, features = [\"napi8\", \"tokio_rt\"] }}\n\
         napi-derive = {{ version = \"3.5.3\", features = [\"type-def\"] }}\n\
         \n\
         [build-dependencies]\n\
         napi-build = \"2.3.1\"\n\
         \n\
         [workspace]\n",
        package_name = package_name,
        core_name = meta.package_name,
        rel_core = rel_core,
        uniffi_dep = render_uniffi_dependency(meta.uniffi_dep.as_ref(), &crate_dir)?,
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
        let rs_path = out_dir.join("node").join(format!("{crate_name}.rs"));
        let rel = relative_path(&src_dir, &rs_path);
        lib_rs.push_str(&format!("include!(\"{rel}\");\n"));
    }
    fs::write(src_dir.join("lib.rs"), lib_rs)?;
    Ok(())
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
