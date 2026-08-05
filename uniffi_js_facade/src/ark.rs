//! Strict ArkTS package printer.
//!
//! This module deliberately owns the ArkTS policy.  It consumes the public AST
//! built by `lib.rs`, sorts every table by its canonical dense ID, and emits a
//! single package-root implementation/declaration pair.  There is no
//! intermediate JavaScript source and no descriptor interpreter in the output:
//! conversion helpers are generated for each operation/type use site.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use uniffi_js_abi::{
    AsyncKind, DefaultValue, ObjectKind, OperationKind, OperationOwner, ScalarType, TypeSourceKey,
    ValueType,
};
use uniffi_js_engine_schema::{
    CallbackReentrancy, CallbackRetention, CallbackThreading, StreamDirection,
};

use crate::{
    custom_type_import_alias, rewrite_custom_type_import, AstCallbackUseSite, AstComponent,
    AstOperation, AstStreamResource, AstType, AstTypeKind, AstVariant, FacadeError, PublicAst,
    PublicFile, PublicFileRole,
};

#[derive(Clone, Debug)]
struct Model<'a> {
    ast: &'a PublicAst,
    components: Vec<&'a AstComponent>,
    types: Vec<&'a AstType>,
    operations: Vec<&'a AstOperation>,
    type_names: BTreeMap<TypeSourceKey, String>,
    component_names: BTreeMap<uniffi_js_abi::ComponentId, String>,
    custom_imports: Vec<String>,
}

impl<'a> Model<'a> {
    fn new(ast: &'a PublicAst) -> Result<Self, FacadeError> {
        let mut components = ast.components.iter().collect::<Vec<_>>();
        components.sort_by_key(|component| component.id);
        let mut types = ast.types.iter().collect::<Vec<_>>();
        types.sort_by_key(|ty| ty.id);
        let mut operations = ast.operations.iter().collect::<Vec<_>>();
        operations.sort_by_key(|operation| operation.id);
        let mut callback_ids = BTreeMap::<TypeSourceKey, BTreeSet<u32>>::new();
        for operation in &operations {
            // Callback contracts are represented by the dedicated callback-owner
            // operations.  TraitBoth also emits an Object/Method operation for
            // the JS -> Rust call path; that operation is not a callback adapter
            // method and must not participate in callback-ID validation.
            let callback_method = operation.kind == OperationKind::CallbackMethod
                && matches!(operation.source_key.owner(), OperationOwner::Callback(_));
            if callback_method {
                let Some(method_id) = operation.callback_method_id else {
                    return Err(FacadeError::MissingCallbackMethodId { id: operation.id });
                };
                let owner = match operation.source_key.owner() {
                    OperationOwner::Object(key)
                    | OperationOwner::Value(key)
                    | OperationOwner::Callback(key) => Some(key),
                    OperationOwner::Namespace => operation.receiver_type.and_then(|id| {
                        ast.types
                            .iter()
                            .find(|ty| ty.id == id)
                            .map(|ty| &ty.source_key)
                    }),
                };
                if let Some(owner) = owner {
                    if !callback_ids
                        .entry(owner.clone())
                        .or_default()
                        .insert(method_id)
                    {
                        return Err(FacadeError::DuplicateCallbackMethodId { id: operation.id });
                    }
                }
            }
        }

        let mut component_names = BTreeMap::new();
        let mut used_component_names = HashSet::new();
        for component in &components {
            let base = safe_ident(&component.namespace);
            let mut name = base.clone();
            let mut suffix = 2usize;
            while !used_component_names.insert(name.clone()) {
                name = format!("{base}_{suffix}");
                suffix += 1;
            }
            component_names.insert(component.id, name);
        }

        // Public names are kept when they are globally unique.  A component
        // prefix is required for a duplicate (including record/object pairs)
        // so one package class/factory can never be shadowed by another owner.
        let mut counts = BTreeMap::<String, usize>::new();
        for ty in &types {
            *counts.entry(ty.name.clone()).or_default() += 1;
        }
        let mut type_names = BTreeMap::new();
        let mut used = HashSet::new();
        let reserved = [
            "ArkValue",
            "ArkRecord",
            "ArkMap",
            "ArkSet",
            "ArkErrorData",
            "ArkCallResult",
            "ArkStreamStep",
            "UniFfiError",
            "BackendSession",
            "Host",
            "CallbackRegistry",
            "ObjectLease",
            "Namespace",
        ];
        for ty in &types {
            let component = safe_ident(ty.source_key.component().namespace());
            let base = safe_ident(&ty.name);
            let mut name = if counts.get(&ty.name).copied().unwrap_or(0) > 1 {
                format!("{component}_{base}")
            } else {
                base
            };
            if reserved.iter().any(|reserved_name| *reserved_name == name) {
                name = format!("{component}_{name}");
            }
            if !used.insert(name.clone()) {
                let mut suffix = 2usize;
                while !used.insert(format!("{name}_{suffix}")) {
                    suffix += 1;
                }
                name = format!("{name}_{suffix}");
            }
            type_names.insert(ty.source_key.clone(), name);
        }

        let mut custom_imports = BTreeSet::new();
        for ty in &types {
            if let AstTypeKind::Custom { config, .. } = &ty.kind {
                let private_name = custom_type_import_alias(ty);
                for import in &config.imports {
                    let line = ark_import_line(import, ty.source_key.component().namespace())
                        .ok_or_else(|| FacadeError::UnsupportedArkImport {
                            import: import.clone(),
                        })?;
                    if !line.is_empty() {
                        custom_imports.insert(
                            rewrite_custom_type_import(
                                &line,
                                &config.public_type_name,
                                &private_name,
                            )
                            .unwrap_or(line),
                        );
                    }
                }
            }
        }

        Ok(Self {
            ast,
            components,
            types,
            operations,
            type_names,
            component_names,
            custom_imports: custom_imports.into_iter().collect(),
        })
    }

    fn type_name(&self, key: &TypeSourceKey) -> String {
        self.type_names
            .get(key)
            .cloned()
            .unwrap_or_else(|| "never".to_owned())
    }

    fn component_name(&self, id: uniffi_js_abi::ComponentId) -> String {
        self.component_names
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("Component{}", id.index()))
    }

    fn ty_for(&self, key: &TypeSourceKey) -> Option<&AstType> {
        self.types.iter().copied().find(|ty| ty.source_key == *key)
    }

    fn operations_for_component(&self, component: &AstComponent) -> Vec<&AstOperation> {
        self.operations
            .iter()
            .copied()
            .filter(|operation| operation.component_id == component.id)
            .collect()
    }

    fn callback_use(
        &self,
        operation_id: uniffi_js_abi::OperationId,
        path: &str,
    ) -> Option<&AstCallbackUseSite> {
        self.ast.callbacks.iter().find(|callback| {
            callback.operation_id == operation_id && callback.path.to_string() == path
        })
    }

    fn stream_resource<'b>(
        &self,
        operation: &'b AstOperation,
        direction: StreamDirection,
    ) -> Result<&'b AstStreamResource, FacadeError> {
        let mut matches = operation
            .stream_resources
            .iter()
            .filter(|resource| resource.direction == direction);
        let Some(resource) = matches.next() else {
            return Err(FacadeError::MissingStreamSlot {
                operation: operation.id,
            });
        };
        if matches.next().is_some() {
            return Err(FacadeError::AmbiguousStreamResource {
                operation: operation.id,
            });
        }
        Ok(resource)
    }
}

#[derive(Default)]
struct Helpers {
    lower: Vec<String>,
    lift: Vec<String>,
    callback: Vec<String>,
    counter: usize,
}

impl Helpers {
    fn next(&mut self, prefix: &str) -> String {
        let name = format!("__ark_{prefix}_{}", self.counter);
        self.counter += 1;
        name
    }
}

pub(crate) fn render_inventory(ast: &PublicAst) -> Result<Vec<PublicFile>, FacadeError> {
    let model = Model::new(ast)?;
    let implementation = render_implementation(&model)?;
    let declaration = render_declaration(&model)?;
    Ok(vec![
        PublicFile::new(
            "Index.ets",
            implementation.into_bytes(),
            PublicFileRole::Implementation,
        ),
        PublicFile::new(
            "Index.d.ets",
            declaration.into_bytes(),
            PublicFileRole::Declaration,
        ),
    ])
}

fn safe_ident(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || character == '_' {
            output.push(character);
        } else {
            output.push('_');
        }
    }
    if output.is_empty() {
        "Component".to_owned()
    } else if output
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_digit())
    {
        format!("_{output}")
    } else {
        output
    }
}

fn ark_import_line(import: &str, component_namespace: &str) -> Option<String> {
    let trimmed = import.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Some(String::new());
    }
    // Custom modules are supplied by the consumer. Preserve the import shape;
    // package-relative paths are rebased below for the ArkTS root facade.
    let line = if trimmed.starts_with("import ") {
        format!("{trimmed};\n")
    } else {
        format!("import {trimmed};\n")
    };
    rebase_ark_import(&line, component_namespace)
}

/// Custom import paths are authored relative to the shared component module
/// (`components/<namespace>/index.js`).  ArkTS has one package-root facade, so
/// resolve the same package file and emit a module specifier relative to
/// `Index.ets`.  Imports which escape the generated source root are rejected:
/// managed custom support must be copied into that atomic root.
fn rebase_ark_import(line: &str, component_namespace: &str) -> Option<String> {
    let quote_end = line.rfind(['"', '\''])?;
    let quote = line.as_bytes()[quote_end] as char;
    let quote_start = line[..quote_end].rfind(quote)?;
    let specifier = &line[quote_start + 1..quote_end];
    if !specifier.starts_with('.') {
        return Some(line.to_owned());
    }

    let mut resolved = vec!["components", component_namespace];
    for segment in specifier.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                resolved.pop()?;
            }
            value => resolved.push(value),
        }
    }
    let rebased = format!("./{}", resolved.join("/"));
    Some(format!(
        "{}{}{}",
        &line[..quote_start + 1],
        rebased,
        &line[quote_end..]
    ))
}

fn render_implementation(model: &Model<'_>) -> Result<String, FacadeError> {
    let mut out = String::new();
    for import in &model.custom_imports {
        out.push_str(import);
    }
    if !model.custom_imports.is_empty() {
        out.push('\n');
    }
    out.push_str(ARK_RUNTIME);
    out.push('\n');
    out.push_str(&format!(
        "const __closePolicy: ClosePolicy = {{ graceMs: {}, onDeadline: \"detach\" }};\n",
        model.ast.close_policy.grace_ms
    ));
    out.push_str(&render_type_definitions(model, false));
    out.push('\n');
    out.push_str(&render_api_declarations(model, false));
    out.push('\n');

    let mut helpers = Helpers::default();
    // Callback adapters are emitted before operation wrappers so generated
    // functions can refer to them without an opaque method table.
    for ty in &model.types {
        if matches!(ty.kind, AstTypeKind::Callback)
            || matches!(
                ty.kind,
                AstTypeKind::Object {
                    kind: ObjectKind::TraitBoth | ObjectKind::TraitForeignOnly
                }
            )
        {
            out.push_str(&render_callback_interface_adapter(model, ty, &mut helpers)?);
            out.push('\n');
        }
    }
    out.push_str(&render_object_classes(model));
    out.push('\n');

    let mut operation_bodies = String::new();
    for operation in &model.operations {
        // Callback methods are invoked through the generated adapter and are
        // not exposed as package namespace functions.
        if matches!(operation.source_key.owner(), OperationOwner::Callback(_)) {
            continue;
        }
        operation_bodies.push_str(&render_operation_helpers(model, operation, &mut helpers)?);
        operation_bodies.push('\n');
    }
    out.push_str(&helpers.lower.join("\n"));
    if !helpers.lower.is_empty() {
        out.push('\n');
    }
    out.push_str(&helpers.lift.join("\n"));
    if !helpers.lift.is_empty() {
        out.push('\n');
    }
    out.push_str(&helpers.callback.join("\n"));
    if !helpers.callback.is_empty() {
        out.push('\n');
    }
    out.push_str(&operation_bodies);
    out.push_str(&render_factory_and_namespace(model));
    Ok(out)
}

fn render_declaration(model: &Model<'_>) -> Result<String, FacadeError> {
    let mut out = String::new();
    for import in &model.custom_imports {
        out.push_str(import);
    }
    if !model.custom_imports.is_empty() {
        out.push('\n');
    }
    out.push_str("// AUTOGENERATED strict ArkTS declaration; package-root facade.\n");
    out.push_str(&render_runtime_declarations());
    out.push('\n');
    out.push_str(&render_type_definitions(model, true));
    out.push('\n');
    out.push_str(&render_api_declarations(model, true));
    out.push('\n');
    out.push_str(&render_object_declarations(model));
    out.push_str("export declare function createNamespace(session: BackendSession): Namespace;\n");
    Ok(out)
}

fn render_type_name(model: &Model<'_>, ty: &ValueType) -> String {
    match ty {
        ValueType::Scalar(scalar) => match scalar {
            ScalarType::Bool => "boolean".to_owned(),
            ScalarType::I64 | ScalarType::U64 => "bigint".to_owned(),
            ScalarType::Bytes => "Uint8Array".to_owned(),
            ScalarType::String => "string".to_owned(),
            _ => "number".to_owned(),
        },
        ValueType::Timestamp => "Date".to_owned(),
        ValueType::Duration => "number".to_owned(),
        ValueType::Named(key) => model.type_name(key),
        ValueType::Optional(inner) => format!("{} | null", render_type_name(model, inner)),
        ValueType::Sequence(inner) => format!("Array<{}>", render_type_name(model, inner)),
        ValueType::Map(key, value) => format!(
            "Map<{}, {}>",
            render_type_name(model, key),
            render_type_name(model, value)
        ),
        ValueType::Set(inner) => format!("Set<{}>", render_type_name(model, inner)),
        ValueType::InputStream { item: inner, .. } => {
            format!("UniFfiInputStream<{}>", render_type_name(model, inner))
        }
        ValueType::OutputStream { item: inner, .. } => {
            format!("UniFfiStream<{}>", render_type_name(model, inner))
        }
    }
}

fn render_type_definitions(model: &Model<'_>, declaration: bool) -> String {
    let mut out = String::new();
    for ty in &model.types {
        let name = model.type_name(&ty.source_key);
        match &ty.kind {
            AstTypeKind::Record { fields } => {
                out.push_str(&format!("export interface {name} {{\n"));
                for field in fields {
                    out.push_str(&format!(
                        "  {}{}: {};\n",
                        field.name,
                        if field.default.is_some() { "?" } else { "" },
                        render_type_name(model, &field.ty)
                    ));
                }
                out.push_str("}\n");
            }
            AstTypeKind::Enum { variants } => {
                render_enum_definition(model, &mut out, ty, name, variants, declaration)
            }
            AstTypeKind::Error { .. } => {
                if declaration {
                    out.push_str(&format!(
                        "export declare class {name} extends UniffiError {{\n  constructor(message?: string, variant?: string | null, data?: ArkValue | null);\n}}\n"
                    ));
                } else {
                    out.push_str(&format!(
                        "export class {name} extends UniffiError {{\n  constructor(message: string = \"\", variant: string | null = null, data: ArkValue | null = null) {{ super(\"{}\", message, variant, data); }}\n}}\n",
                        name
                    ));
                }
            }
            AstTypeKind::Custom { config, builtin } => {
                let private_name = custom_type_import_alias(ty);
                let uses_import_alias = config.imports.iter().any(|import| {
                    ark_import_line(import, ty.source_key.component().namespace()).is_some_and(
                        |line| {
                            rewrite_custom_type_import(
                                &line,
                                &config.public_type_name,
                                &private_name,
                            )
                            .is_some()
                        },
                    )
                });
                let public_type = if uses_import_alias {
                    private_name
                } else if config.public_type_name.is_empty() {
                    render_type_name(model, builtin)
                } else {
                    config.public_type_name.clone()
                };
                out.push_str(&format!("export type {name} = {public_type};\n"));
            }
            AstTypeKind::Object {
                kind: ObjectKind::TraitForeignOnly,
            } => {
                out.push_str(&format!("export interface {name} {{\n"));
                for operation in model.operations.iter().copied().filter(|operation| {
                    matches!(operation.source_key.owner(), OperationOwner::Callback(key) if key == &ty.source_key && operation.kind == OperationKind::CallbackMethod)
                }) {
                    out.push_str(&format_callback_signature(model, operation));
                }
                out.push_str("}\n");
            }
            AstTypeKind::Object { .. } => {}
            AstTypeKind::Callback => {
                out.push_str(&format!("export interface {name} {{\n"));
                for operation in model.operations.iter().copied().filter(|operation| {
                    matches!(operation.source_key.owner(), OperationOwner::Callback(key) if key == &ty.source_key && operation.kind == OperationKind::CallbackMethod)
                }) {
                    out.push_str(&format_callback_signature(model, operation));
                }
                out.push_str("}\n");
            }
        }
    }
    out
}

fn render_enum_definition(
    model: &Model<'_>,
    out: &mut String,
    ty: &AstType,
    name: String,
    variants: &[AstVariant],
    declaration: bool,
) {
    let variant_names = variants
        .iter()
        .map(|variant| format!("{name}_{}", safe_ident(&variant.name)))
        .collect::<Vec<_>>();
    out.push_str(&format!(
        "export type {name} = {};\n",
        variant_names.join(" | ")
    ));
    for variant in variants {
        let variant_name = format!("{name}_{}", safe_ident(&variant.name));
        out.push_str(&format!(
            "export interface {variant_name} {{\n  readonly tag: \"{}\";\n",
            variant.name
        ));
        for field in &variant.fields {
            out.push_str(&format!(
                "  readonly {}: {};\n",
                field.name,
                render_type_name(model, &field.ty)
            ));
        }
        out.push_str("}\n");
        if !variant.fields.is_empty() {
            out.push_str(&format!("export interface {variant_name}Input {{\n"));
            for field in &variant.fields {
                out.push_str(&format!(
                    "  readonly {}: {};\n",
                    field.name,
                    render_type_name(model, &field.ty)
                ));
            }
            out.push_str("}\n");
        }
    }
    out.push_str(&format!("export interface {name}Value {{\n"));
    for variant in variants {
        let variant_name = format!("{name}_{}", safe_ident(&variant.name));
        if variant.fields.is_empty() {
            out.push_str(&format!("  readonly {}: {variant_name};\n", variant.name));
        } else {
            out.push_str(&format!(
                "  readonly {}: (value: {variant_name}Input) => {variant_name};\n",
                variant.name
            ));
        }
    }
    for operation in model.operations.iter().copied().filter(|operation| {
        matches!(operation.source_key.owner(), OperationOwner::Value(key) if key == &ty.source_key)
    }) {
        out.push_str(&format_api_value_operation_signature(
            model, operation, "  ",
        ));
    }
    out.push_str("}\n");
    // ArkTS forbids a type alias and value declaration from sharing a name.
    // Keep the enum value namespace package-local; public consumers reach it
    // through the component API (`namespace.<component>.{name}`).
    if declaration {
        return;
    }
    let value_name = format!("__arkEnum_{}", safe_ident(&name));
    let variants_type = format!("__ArkEnum{}Variants", safe_ident(&name));
    out.push_str(&format!("interface {variants_type} {{\n"));
    for variant in variants {
        let variant_name = format!("{name}_{}", safe_ident(&variant.name));
        if variant.fields.is_empty() {
            out.push_str(&format!("  readonly {}: {variant_name};\n", variant.name));
        } else {
            out.push_str(&format!(
                "  readonly {}: (value: {variant_name}Input) => {variant_name};\n",
                variant.name
            ));
        }
    }
    out.push_str("}\n");
    out.push_str(&format!("const {value_name}: {variants_type} = {{\n"));
    for variant in variants {
        let variant_name = format!("{name}_{}", safe_ident(&variant.name));
        if variant.fields.is_empty() {
            out.push_str(&format!(
                "  {}: {{ tag: \"{}\" }} as {},\n",
                variant.name, variant.name, variant_name
            ));
        } else {
            out.push_str(&format!(
                "  {}: (value: {variant_name}Input): {variant_name} => {{ const result: {variant_name} = {{ tag: \"{}\"",
                variant.name, variant.name
            ));
            for field in &variant.fields {
                out.push_str(&format!(", {}: value.{}", field.name, field.name));
            }
            out.push_str(" }; return result; },\n");
        }
    }
    out.push_str("};\n");
}

fn format_callback_signature(model: &Model<'_>, operation: &AstOperation) -> String {
    let args = operation
        .arguments
        .iter()
        .map(|argument| {
            format!(
                "{}: {}",
                argument.name,
                render_type_name(model, &argument.ty)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let return_type = operation
        .return_type
        .as_ref()
        .map(|value| render_type_name(model, value))
        .unwrap_or_else(|| "void".to_owned());
    let return_type = if operation.async_kind == AsyncKind::Async {
        format!("Promise<{return_type}>")
    } else {
        return_type
    };
    format!("  {}({args}): {return_type};\n", operation.name)
}

fn render_api_declarations(model: &Model<'_>, declaration: bool) -> String {
    let mut out = String::new();
    for component in &model.components {
        let component_name = model.component_name(component.id);
        out.push_str(&format!("export interface {component_name}Api {{\n"));
        let operations = model.operations_for_component(component);
        for operation in operations.iter().copied().filter(|operation| {
            operation.receiver_type.is_none()
                && matches!(operation.source_key.owner(), OperationOwner::Namespace)
                && !matches!(
                    operation.kind,
                    OperationKind::OutputStreamNext
                        | OperationKind::OutputStreamCancel
                        | OperationKind::InputStreamPull
                        | OperationKind::InputStreamCancel
                )
        }) {
            out.push_str(&format_api_operation_signature(model, operation, "  "));
        }
        for ty in model
            .types
            .iter()
            .copied()
            .filter(|ty| ty.source_key.component().namespace() == component.namespace)
        {
            let value_ops = model
                .operations
                .iter()
                .copied()
                .filter(|operation| {
                    matches!(operation.source_key.owner(), OperationOwner::Value(key) if key == &ty.source_key)
                })
                .collect::<Vec<_>>();
            if !value_ops.is_empty() || matches!(ty.kind, AstTypeKind::Enum { .. }) {
                out.push_str(&format!(
                    "  readonly {}: {}Value;\n",
                    model.type_name(&ty.source_key),
                    model.type_name(&ty.source_key)
                ));
            }
            if matches!(ty.kind, AstTypeKind::Object { .. }) {
                let constructors = model
                    .operations
                    .iter()
                    .copied()
                    .filter(|operation| {
                        operation.kind == OperationKind::Constructor
                            && matches!(operation.source_key.owner(), OperationOwner::Object(key) if key == &ty.source_key)
                    })
                    .collect::<Vec<_>>();
                if !constructors.is_empty() {
                    out.push_str(&format!(
                        "  readonly {}: {}Constructor;\n",
                        model.type_name(&ty.source_key),
                        model.type_name(&ty.source_key)
                    ));
                }
            }
        }
        out.push_str("}\n");

        // Value and constructor interfaces are owner-scoped through the
        // component API.  Their type names are canonical owner names even
        // when a foreign component uses them in a signature.
        for ty in model
            .types
            .iter()
            .copied()
            .filter(|ty| ty.source_key.component().namespace() == component.namespace)
        {
            let value_ops = model
                .operations
                .iter()
                .copied()
                .filter(|operation| {
                    matches!(operation.source_key.owner(), OperationOwner::Value(key) if key == &ty.source_key)
                })
                .collect::<Vec<_>>();
            if !value_ops.is_empty() && !matches!(ty.kind, AstTypeKind::Enum { .. }) {
                out.push_str(&format!(
                    "export interface {}Value {{\n",
                    model.type_name(&ty.source_key)
                ));
                for operation in value_ops {
                    out.push_str(&format_api_value_operation_signature(
                        model, operation, "  ",
                    ));
                }
                out.push_str("}\n");
            }
            if matches!(ty.kind, AstTypeKind::Object { .. }) {
                let constructors = model
                    .operations
                    .iter()
                    .copied()
                    .filter(|operation| {
                        operation.kind == OperationKind::Constructor
                            && matches!(operation.source_key.owner(), OperationOwner::Object(key) if key == &ty.source_key)
                    })
                    .collect::<Vec<_>>();
                if !constructors.is_empty() {
                    out.push_str(&format!(
                        "export interface {}Constructor {{\n",
                        model.type_name(&ty.source_key)
                    ));
                    for operation in constructors {
                        out.push_str(&format_api_constructor_signature(
                            model,
                            operation,
                            "  ",
                            &model.type_name(&ty.source_key),
                        ));
                    }
                    out.push_str("}\n");
                }
            }
        }
    }
    out.push_str("export interface Namespace {\n");
    for component in &model.components {
        let name = model.component_name(component.id);
        out.push_str(&format!("  readonly {name}: {name}Api;\n"));
    }
    out.push_str("}\n");
    let _ = declaration;
    out
}

fn format_operation_signature(model: &Model<'_>, operation: &AstOperation, indent: &str) -> String {
    let args = operation
        .arguments
        .iter()
        .map(|argument| {
            format!(
                "{}{}: {}",
                argument.name,
                if argument.default.is_some() { "?" } else { "" },
                render_type_name(model, &argument.ty)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let return_type = operation_return_type(model, operation);
    format!("{indent}{}({args}): {return_type};\n", operation.name)
}

fn format_api_operation_signature(
    model: &Model<'_>,
    operation: &AstOperation,
    indent: &str,
) -> String {
    let args = operation
        .arguments
        .iter()
        .map(|argument| {
            format!(
                "{}{}: {}",
                argument.name,
                if argument.default.is_some() { "?" } else { "" },
                render_type_name(model, &argument.ty)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{indent}readonly {}: ({args}) => {};\n",
        operation.name,
        operation_return_type(model, operation)
    )
}

fn format_api_value_operation_signature(
    model: &Model<'_>,
    operation: &AstOperation,
    indent: &str,
) -> String {
    let owner = match operation.source_key.owner() {
        OperationOwner::Value(key) => model.type_name(key),
        _ => return format_api_operation_signature(model, operation, indent),
    };
    let mut args = if operation.kind == OperationKind::Constructor {
        Vec::new()
    } else {
        vec![format!("self_: {owner}")]
    };
    args.extend(operation.arguments.iter().map(|argument| {
        format!(
            "{}{}: {}",
            argument.name,
            if argument.default.is_some() { "?" } else { "" },
            render_type_name(model, &argument.ty)
        )
    }));
    format!(
        "{indent}readonly {}: ({}) => {};\n",
        operation.name,
        args.join(", "),
        operation_return_type(model, operation)
    )
}

fn format_api_constructor_signature(
    model: &Model<'_>,
    operation: &AstOperation,
    indent: &str,
    object_name: &str,
) -> String {
    let args = operation
        .arguments
        .iter()
        .map(|argument| {
            format!(
                "{}{}: {}",
                argument.name,
                if argument.default.is_some() { "?" } else { "" },
                render_type_name(model, &argument.ty)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let return_type = if operation.async_kind == AsyncKind::Async {
        format!("Promise<{object_name}>")
    } else {
        object_name.to_owned()
    };
    format!(
        "{indent}readonly {}: ({args}) => {return_type};\n",
        operation.name
    )
}

fn operation_return_type(model: &Model<'_>, operation: &AstOperation) -> String {
    let return_type = operation
        .return_type
        .as_ref()
        .map(|value| render_type_name(model, value))
        .unwrap_or_else(|| "void".to_owned());
    if operation.async_kind == AsyncKind::Async {
        format!("Promise<{return_type}>")
    } else {
        return_type
    }
}

fn render_object_classes(model: &Model<'_>) -> String {
    let mut out = String::new();
    for ty in &model.types {
        if !matches!(ty.kind, AstTypeKind::Object { .. })
            || matches!(
                ty.kind,
                AstTypeKind::Object {
                    kind: ObjectKind::TraitForeignOnly
                }
            )
        {
            continue;
        }
        let name = model.type_name(&ty.source_key);
        out.push_str(&format!(
            "export class {name} extends ObjectLease {{\n  private constructor(token: ArkObjectToken, handle: ArkValue) {{ super(token.session, handle, {}); }}\n  static __arkCreate(token: ArkObjectToken, handle: ArkValue): {name} {{ if (token.ownerTypeId !== {}) throw new UniffiError(\"UniffiObjectOwner\", \"object token owner mismatch\"); return new {name}(token, handle); }}\n",
            ty.id.index(),
            ty.id.index()
        ));
        for operation in model.operations.iter().copied().filter(|operation| {
            operation.receiver_type == Some(ty.id)
                && !matches!(
                    operation.kind,
                    OperationKind::OutputStreamNext
                        | OperationKind::OutputStreamCancel
                        | OperationKind::InputStreamPull
                        | OperationKind::InputStreamCancel
                )
        }) {
            out.push_str(
                &format_operation_signature(model, operation, "  ").replace(";\n", " {\n"),
            );
            let args = operation
                .arguments
                .iter()
                .map(|argument| argument.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            let call_args = if args.is_empty() {
                String::new()
            } else {
                format!(", {args}")
            };
            out.push_str(&format!(
                "    return __invokeObject{}(this, this.objectSession(), (): ArkValue => this.objectHandle(){call_args});\n  }}\n",
                operation.id.index()
            ));
        }
        out.push_str("}\n");
    }
    out
}

fn render_object_declarations(model: &Model<'_>) -> String {
    let mut out = String::new();
    for ty in &model.types {
        if !matches!(ty.kind, AstTypeKind::Object { .. })
            || matches!(
                ty.kind,
                AstTypeKind::Object {
                    kind: ObjectKind::TraitForeignOnly
                }
            )
        {
            continue;
        }
        let name = model.type_name(&ty.source_key);
        out.push_str(&format!("export declare class {name} extends ObjectLease {{\n  private constructor(token: object, handle: ArkValue);\n"));
        for operation in model.operations.iter().copied().filter(|operation| {
            operation.receiver_type == Some(ty.id)
                && !matches!(
                    operation.kind,
                    OperationKind::OutputStreamNext
                        | OperationKind::OutputStreamCancel
                        | OperationKind::InputStreamPull
                        | OperationKind::InputStreamCancel
                )
        }) {
            out.push_str(&format_operation_signature(model, operation, "  "));
        }
        out.push_str("}\n");
    }
    out
}

const ARK_RUNTIME: &str = r#"// AUTOGENERATED strict ArkTS runtime; package-local and self-contained.
interface ClosePolicy { readonly graceMs: number; readonly onDeadline: "detach"; }
class __ArkDetachedMarker {}
const __DETACHED: __ArkDetachedMarker = new __ArkDetachedMarker();
interface __ClosePolicyState { policy: ClosePolicy | null; installed: boolean; open: boolean; }
const __CLOSE_POLICY_STATES: WeakMap<BackendSession, __ClosePolicyState> = new WeakMap<BackendSession, __ClosePolicyState>();
function __validateClosePolicy(policy: ClosePolicy): ClosePolicy {
  if (policy === null) throw new UniffiError("UniffiClosePolicy", "close policy must be an object");
  if (!Number.isFinite(policy.graceMs) || !Number.isInteger(policy.graceMs) || policy.graceMs < 0) throw new UniffiError("UniffiClosePolicy", "close policy graceMs must be a finite non-negative integer");
  if (policy.onDeadline !== "detach") throw new UniffiError("UniffiClosePolicy", "close policy onDeadline must be detach");
  const validated: ClosePolicy = { graceMs: policy.graceMs, onDeadline: "detach" };
  return validated;
}
function __sameClosePolicy(left: ClosePolicy, right: ClosePolicy): boolean { return left.graceMs === right.graceMs && left.onDeadline === right.onDeadline; }
function __sessionClosePolicy(session: BackendSession): ClosePolicy { const state: __ClosePolicyState | undefined = __CLOSE_POLICY_STATES.get(session); if (state === undefined || state.policy === null) throw new UniffiError("UniffiClosePolicy", "generated facade did not install close policy"); return state.policy; }
function __installClosePolicy(session: BackendSession, policy: ClosePolicy): void {
  const state: __ClosePolicyState | undefined = __CLOSE_POLICY_STATES.get(session);
  if (state === undefined) throw new UniffiError("UniffiSessionType", "invalid BackendSession");
  const validated: ClosePolicy = __validateClosePolicy(policy);
  if (!state.open) { if (state.policy === null || !__sameClosePolicy(state.policy, validated)) throw new UniffiError("UniffiClosePolicy", "cannot change close policy after session teardown starts"); return; }
  if (state.installed && (state.policy === null || !__sameClosePolicy(state.policy, validated))) throw new UniffiError("UniffiClosePolicy", "session close policy does not match generated facade policy");
  state.policy = validated;
  state.installed = true;
}
function __markPolicyClosed(session: BackendSession): void { const state: __ClosePolicyState | undefined = __CLOSE_POLICY_STATES.get(session); if (state !== undefined) state.open = false; }
export type ArkPrimitive = string | number | boolean | bigint;
export class ArkRecord {
  private readonly values: Map<string, ArkValue> = new Map<string, ArkValue>();
  set(name: string, value: ArkValue): void { this.values.set(name, value); }
  has(name: string): boolean { return this.values.has(name); }
  get(name: string): ArkValue {
    const value: ArkValue | undefined = this.values.get(name);
    if (value === undefined) throw new UniffiError("UniffiRecordField", "missing record field " + name);
    return value;
  }
}
export type ArkValue = ArkPrimitive | Uint8Array | Date | ArkRecord | Array<ArkValue> | Map<ArkValue, ArkValue> | Set<ArkValue> | null;
export interface ArkFailure {
  readonly errorName: string;
  readonly message: string;
  readonly variant: string | null;
  readonly data: ArkValue | null;
}
export interface ArkCallbackResult {
  readonly ok: boolean;
  readonly value?: ArkValue;
  readonly error?: ArkFailure;
}
export class ArkValueResult { readonly kind: "value" = "value"; readonly value: ArkValue; constructor(value: ArkValue) { this.value = value; } }
export class ArkErrorResult { readonly kind: "error" = "error"; readonly error: ArkFailure; constructor(error: ArkFailure) { this.error = error; } }
export type ArkCallResult = ArkValueResult | ArkErrorResult;
export interface ArkBackend {
  invokeSync(operationId: number, args: Array<ArkValue>): ArkCallResult;
  invokeAsync(operationId: number, args: Array<ArkValue>): Promise<ArkCallResult>;
  releaseObject(handle: ArkValue): void;
  releaseOutputStream(handle: ArkValue): void;
  close?(): Promise<void> | void;
}
export class UniffiError extends Error {
  readonly errorName: string;
  readonly variant: string | null;
  readonly data: ArkValue | null;
  constructor(errorName: string = "UniffiUnknownError", message: string = "", variant: string | null = null, data: ArkValue | null = null) {
    super(message);
    this.name = "UniffiError";
    this.errorName = errorName;
    this.variant = variant;
    this.data = data;
  }
  static fromFailure(failure: ArkFailure): UniffiError {
    return new UniffiError(failure.errorName, failure.message, failure.variant, failure.data);
  }
}
export class ArkItemStep<T> { readonly kind: "item" = "item"; readonly value: T; constructor(value: T) { this.value = value; } }
export class ArkDoneStep { readonly kind: "done" = "done"; }
export class ArkErrorStep { readonly kind: "error" = "error"; readonly error: UniffiError; constructor(error: UniffiError) { this.error = error; } }
export type ArkStreamStep<T> = ArkItemStep<T> | ArkDoneStep | ArkErrorStep;
export interface UniFfiStream<T> {
  next(): Promise<ArkStreamStep<T>>;
  cancel(): Promise<void>;
}
export interface UniFfiInputStream<T> {
  next(): Promise<ArkStreamStep<T>>;
  cancel(): Promise<void>;
  release(): void;
}
export interface ArkCallbackAdapter {
  invokeSync(methodId: number, args: Array<ArkValue>): ArkValue;
  invokeAsync(methodId: number, invocationId: number, args: Array<ArkValue>): Promise<ArkValue>;
}
export interface ArkCallbackContract {
  readonly retention: "scoped" | "retained";
  readonly threading: "callingThread" | "mayCrossThread";
  readonly reentrancy: "forbidden" | "allowed";
}
interface ArkCallbackEntry {
  readonly callback: ArkCallbackAdapter;
  readonly contract: ArkCallbackContract;
  frameRefs: number;
  leases: number;
  hostLeases: number;
  activeSync: boolean;
  readonly activeAsync: Set<number>;
}
export class CallbackRegistry {
  private readonly session: BackendSession | null;
  private readonly entries: Map<number, Map<number, ArkCallbackEntry>> = new Map<number, Map<number, ArkCallbackEntry>>();
  private readonly nextIds: Map<number, number> = new Map<number, number>();
  constructor(session: BackendSession | null = null) { this.session = session; }
  private bucket(callbackType: number, create: boolean): Map<number, ArkCallbackEntry> | null {
    const current: Map<number, ArkCallbackEntry> | undefined = this.entries.get(callbackType);
    if (current !== undefined) return current;
    if (!create) return null;
    const created: Map<number, ArkCallbackEntry> = new Map<number, ArkCallbackEntry>();
    this.entries.set(callbackType, created);
    return created;
  }
  register(callbackType: number, callback: ArkCallbackAdapter, contract: ArkCallbackContract): number {
    const bucket: Map<number, ArkCallbackEntry> = this.bucket(callbackType, true) as Map<number, ArkCallbackEntry>;
    const current: number | undefined = this.nextIds.get(callbackType);
    const id: number = current === undefined ? 1 : current;
    this.nextIds.set(callbackType, id + 1);
    const entry: ArkCallbackEntry = {
      callback,
      contract,
      frameRefs: 0,
      leases: 0,
      hostLeases: 0,
      activeSync: false,
      activeAsync: new Set<number>(),
    };
    bucket.set(id, entry);
    return id;
  }
  private entry(callbackType: number, callbackId: number): ArkCallbackEntry {
    const bucket: Map<number, ArkCallbackEntry> | null = this.bucket(callbackType, false);
    const value: ArkCallbackEntry | undefined = bucket === null ? undefined : bucket.get(callbackId);
    if (value === undefined) throw new UniffiError("UniffiCallbackMissing", "callback is not registered");
    return value;
  }
  private cleanup(callbackType: number, callbackId: number, entry: ArkCallbackEntry): void {
    if (entry.frameRefs !== 0 || entry.leases !== 0 || entry.hostLeases !== 0 || entry.activeSync || entry.activeAsync.size !== 0) return;
    const bucket: Map<number, ArkCallbackEntry> | null = this.bucket(callbackType, false);
    if (bucket === null) return;
    bucket.delete(callbackId);
    if (bucket.size === 0) this.entries.delete(callbackType);
  }
  retain(callbackType: number, callbackId: number): ArkCallbackLease {
    const entry: ArkCallbackEntry = this.entry(callbackType, callbackId);
    entry.leases += 1;
    let released: boolean = false;
    return new ArkCallbackLease((): void => {
      if (released) return;
      released = true;
      entry.leases -= 1;
      this.cleanup(callbackType, callbackId, entry);
    });
  }
  release(callbackType: number, callbackId: number): void {
    const bucket: Map<number, ArkCallbackEntry> | null = this.bucket(callbackType, false);
    const entry: ArkCallbackEntry | undefined = bucket === null ? undefined : bucket.get(callbackId);
    if (entry === undefined) return;
    if (entry.leases > 0) entry.leases -= 1;
    this.cleanup(callbackType, callbackId, entry);
  }
  hostRetain(callbackType: number, callbackId: number): void { this.entry(callbackType, callbackId).hostLeases += 1; }
  hostRelease(callbackType: number, callbackId: number): void {
    const bucket: Map<number, ArkCallbackEntry> | null = this.bucket(callbackType, false);
    const entry: ArkCallbackEntry | undefined = bucket === null ? undefined : bucket.get(callbackId);
    if (entry === undefined) return;
    if (entry.hostLeases > 0) entry.hostLeases -= 1;
    this.cleanup(callbackType, callbackId, entry);
  }
  frameRetain(callbackType: number, callbackId: number): void { this.entry(callbackType, callbackId).frameRefs += 1; }
  frameRelease(callbackType: number, callbackId: number): void {
    const bucket: Map<number, ArkCallbackEntry> | null = this.bucket(callbackType, false);
    const entry: ArkCallbackEntry | undefined = bucket === null ? undefined : bucket.get(callbackId);
    if (entry === undefined) return;
    if (entry.frameRefs > 0) entry.frameRefs -= 1;
    this.cleanup(callbackType, callbackId, entry);
  }
  invokeSync(callbackType: number, callbackId: number, methodId: number, args: Array<ArkValue>): ArkValue {
    this.session?.assertCallbackOpen();
    const entry: ArkCallbackEntry = this.entry(callbackType, callbackId);
    if (entry.contract.reentrancy === "forbidden" && (entry.activeSync || entry.activeAsync.size > 0)) {
      throw new UniffiError("UniffiCallbackReentrancy", "forbidden callback reentrancy");
    }
    entry.activeSync = true;
    try { return entry.callback.invokeSync(methodId, args); }
    finally { entry.activeSync = false; this.cleanup(callbackType, callbackId, entry); }
  }
  async invokeAsync(callbackType: number, callbackId: number, methodId: number, invocationId: number, args: Array<ArkValue>): Promise<ArkValue> {
    this.session?.assertCallbackOpen();
    const entry: ArkCallbackEntry = this.entry(callbackType, callbackId);
    if (entry.activeAsync.has(invocationId)) throw new UniffiError("UniffiCallbackInvocation", "duplicate callback invocation");
    if (entry.contract.reentrancy === "forbidden" && (entry.activeSync || entry.activeAsync.size > 0)) {
      throw new UniffiError("UniffiCallbackReentrancy", "forbidden callback reentrancy");
    }
    entry.activeAsync.add(invocationId);
    const generation: number | undefined = this.session?.generation;
    try {
      const result: ArkValue | Promise<ArkValue> = entry.callback.invokeAsync(methodId, invocationId, args);
      const guarded: ArkValue | __ArkDetachedMarker = this.session === null
        ? await result
        : await this.session.guardPromise(result, generation as number);
      if (guarded === __DETACHED) throw new UniffiError("UniffiSessionClosed", "backend session is closed");
      return guarded as ArkValue;
    }
      finally { entry.activeAsync.delete(invocationId); this.cleanup(callbackType, callbackId, entry); }
  }
  detach(): void { this.entries.clear(); }
}
export class ArkCallbackLease {
  private readonly releaseFunction: () => void;
  constructor(releaseFunction: () => void) { this.releaseFunction = releaseFunction; }
  release(): void { this.releaseFunction(); }
}
export class Host {
  private callbackRegistry: CallbackRegistry | null = null;
  private inputRegistry: ArkInputRegistry | null = null;
  private boundSession: BackendSession | null = null;
  attachRegistry(registry: CallbackRegistry, session: BackendSession): void {
    if (this.boundSession !== null && this.boundSession !== session) throw new UniffiError("UniffiHostSession", "host is already bound to another session");
    if (this.callbackRegistry !== null && this.callbackRegistry !== registry) throw new UniffiError("UniffiHostSession", "callback registry is already attached");
    this.boundSession = session;
    this.callbackRegistry = registry;
  }
  attachInputRegistry(registry: ArkInputRegistry, session: BackendSession): void {
    if (this.boundSession !== null && this.boundSession !== session) throw new UniffiError("UniffiHostSession", "host is already bound to another session");
    if (this.inputRegistry !== null && this.inputRegistry !== registry) throw new UniffiError("UniffiHostSession", "input registry is already attached");
    this.boundSession = session;
    this.inputRegistry = registry;
  }
  retainCallback(callbackType: number, callbackId: number): void { this.boundSession?.assertCallbackOpen(); if (this.callbackRegistry === null) throw new UniffiError("UniffiCallbackMissing", "callback registry is not attached"); this.callbackRegistry.hostRetain(callbackType, callbackId); }
  releaseCallback(callbackType: number, callbackId: number): void { if (this.boundSession?.isDetached()) return; this.callbackRegistry?.hostRelease(callbackType, callbackId); }
  invokeCallbackSync(callbackType: number, callbackId: number, methodId: number, args: Array<ArkValue> = []): ArkValue {
    this.boundSession?.assertCallbackOpen();
    if (this.callbackRegistry === null) throw new UniffiError("UniffiCallbackMissing", "callback registry is not attached");
    return this.callbackRegistry.invokeSync(callbackType, callbackId, methodId, args);
  }
  invokeCallbackAsync(callbackType: number, callbackId: number, methodId: number, invocationId: number, args: Array<ArkValue> = []): Promise<ArkValue> {
    try { this.boundSession?.assertCallbackOpen(); } catch (error) { return Promise.reject(error); }
    if (this.callbackRegistry === null) return Promise.reject(new UniffiError("UniffiCallbackMissing", "callback registry is not attached"));
    return this.callbackRegistry.invokeAsync(callbackType, callbackId, methodId, invocationId, args);
  }
  invokeCallbackSyncResult(callbackType: number, callbackId: number, methodId: number, args: Array<ArkValue> = []): ArkCallbackResult {
    try { return { ok: true, value: this.invokeCallbackSync(callbackType, callbackId, methodId, args) }; }
    catch (error) { return { ok: false, error: __arkCallbackFailure(error) }; }
  }
  async invokeCallbackAsyncResult(callbackType: number, callbackId: number, methodId: number, invocationId: number, args: Array<ArkValue> = []): Promise<ArkCallbackResult> {
    try { return { ok: true, value: await this.invokeCallbackAsync(callbackType, callbackId, methodId, invocationId, args) }; }
    catch (error) { return { ok: false, error: __arkCallbackFailure(error) }; }
  }
  pullInputStream(handle: ArkValue): Promise<ArkStreamStep<ArkValue>> {
    if (this.inputRegistry === null) return Promise.reject(new UniffiError("UniffiInputStreamMissing", "input stream registry is not attached"));
    return this.inputRegistry.pull(handle);
  }
  cancelInputStream(handle: ArkValue): Promise<void> {
    if (this.inputRegistry === null) return Promise.resolve();
    return this.inputRegistry.cancel(handle);
  }
  releaseInputStream(handle: ArkValue): void { this.inputRegistry?.release(handle); }
}
function __arkCallbackFailure(error: Error): ArkFailure {
  if (error instanceof UniffiError) return { errorName: error.errorName, message: error.message, variant: error.variant, data: error.data };
  return { errorName: error.name, message: error.message, variant: null, data: null };
}
interface ArkInputSlot { readonly pull: () => Promise<ArkStreamStep<ArkValue>>; readonly cancel: () => Promise<void>; readonly release: () => void; readonly detach: () => void; }
class ArkInputRegistry {
  private nextHandle: number = 1;
  private readonly slots: Map<number, ArkInputSlot> = new Map<number, ArkInputSlot>();
  register<T>(source: UniFfiInputStream<T>, lower: (value: T) => ArkValue): number {
    const handle: number = this.nextHandle;
    this.nextHandle += 1;
    let pending: boolean = false;
    let closed: boolean = false;
    let detached: boolean = false;
    let removed: boolean = false;
    const remove: () => void = (): void => {
      if (removed) return;
      removed = true;
      closed = true;
      this.slots.delete(handle);
    };
    const slot: ArkInputSlot = {
      pull: async (): Promise<ArkStreamStep<ArkValue>> => {
        if (closed) return new ArkDoneStep();
        if (pending) throw new UniffiError("UniffiInputStreamConcurrentNext", "concurrent input stream pull");
        pending = true;
        try {
          const step: ArkStreamStep<T> = await source.next();
          if (closed || detached) return new ArkDoneStep();
          if (step.kind === "item") return new ArkItemStep<ArkValue>(lower(step.value));
          if (step.kind === "done") { remove(); return new ArkDoneStep(); }
          remove();
          return new ArkErrorStep(step.error);
        } catch (error) {
          remove();
          if (detached) return new ArkDoneStep();
          if (error instanceof UniffiError) return new ArkErrorStep(error);
          return new ArkErrorStep(new UniffiError("UniffiInputStreamError", "input stream failed"));
        } finally { pending = false; }
      },
      cancel: async (): Promise<void> => {
        if (removed) return;
        closed = true;
        if (detached) { remove(); return; }
        try { await source.cancel(); }
        finally { remove(); }
      },
      release: (): void => { remove(); },
      detach: (): void => { detached = true; closed = true; },
    };
    this.slots.set(handle, slot);
    return handle;
  }
  pull(handle: ArkValue): Promise<ArkStreamStep<ArkValue>> {
    const numericHandle: number = handle as number;
    const slot: ArkInputSlot | undefined = this.slots.get(numericHandle);
    if (slot === undefined) return Promise.reject(new UniffiError("UniffiInputStreamMissing", "input stream handle is not registered"));
    return slot.pull();
  }
  cancel(handle: ArkValue): Promise<void> {
    const numericHandle: number = handle as number;
    const slot: ArkInputSlot | undefined = this.slots.get(numericHandle);
    return slot === undefined ? Promise.resolve() : slot.cancel();
  }
  release(handle: ArkValue): void {
    const numericHandle: number = handle as number;
    const slot: ArkInputSlot | undefined = this.slots.get(numericHandle);
    if (slot !== undefined) slot.release();
  }
  close(): Promise<void> {
    const pending: Array<Promise<void>> = [];
    for (const slot of this.slots.values()) pending.push(slot.cancel());
    return Promise.all(pending).then((): void => undefined);
  }
  detach(): void { for (const slot of this.slots.values()) slot.detach(); this.slots.clear(); }
}
class ArkInputView<T> implements UniFfiInputStream<T> {
  private readonly session: BackendSession;
  private readonly handle: ArkValue;
  private readonly lift: (value: ArkValue) => T;
  private released: boolean = false;
  constructor(session: BackendSession, handle: ArkValue, lift: (value: ArkValue) => T) { this.session = session; this.handle = handle; this.lift = lift; }
  async next(): Promise<ArkStreamStep<T>> {
    const step: ArkStreamStep<ArkValue> = await this.session.pullInputStream(this.handle);
    if (step.kind === "item") return new ArkItemStep<T>(this.lift(step.value));
    this.release();
    if (step.kind === "done") return new ArkDoneStep();
    return new ArkErrorStep(step.error);
  }
  async cancel(): Promise<void> { try { await this.session.cancelInputStream(this.handle); } finally { this.release(); } }
  release(): void { if (this.released) return; this.released = true; this.session.releaseInputStream(this.handle); }
}
export interface ArkOutputSpec<T> {
  readonly start: () => Promise<ArkValue> | ArkValue;
  readonly next: (handle: ArkValue) => Promise<ArkCallResult>;
  readonly cancel: (handle: ArkValue) => Promise<void>;
  readonly lift: (value: ArkValue) => T;
  readonly error: (value: ArkValue) => UniffiError;
  readonly release: (handle: ArkValue) => void;
  readonly onClose?: () => void;
}
class ArkOutputStream<T> implements UniFfiStream<T> {
  private readonly session: BackendSession;
  private readonly spec: ArkOutputSpec<T>;
  private phase: "idle" | "starting" | "active" | "done" | "cancelled" = "idle";
  private handle: ArkValue | null = null;
  private pending: boolean = false;
  private claimed: boolean = false;
  private released: boolean = false;
  private cancelRequested: boolean = false;
  private cancelIssued: boolean = false;
  private detached: boolean = false;
  constructor(session: BackendSession, spec: ArkOutputSpec<T>) { this.session = session; this.spec = spec; }
  private finish(): void {
    if (this.released) return;
    this.released = true;
    if (this.handle !== null) this.spec.release(this.handle);
    if (!this.detached) this.spec.onClose?.();
  }
  private async abort(reason: UniffiError): Promise<UniffiError> {
    if (this.handle !== null && !this.cancelIssued) {
      this.cancelIssued = true;
      try { await this.spec.cancel(this.handle); }
      catch (error) { if (error instanceof UniffiError) reason = error; }
    }
    this.phase = "done";
    this.finish();
    return reason;
  }
  async next(): Promise<ArkStreamStep<T>> {
    if (this.claimed && this.pending) return Promise.reject(new UniffiError("UniffiStreamConcurrentNext", "concurrent output stream pull"));
    this.claimed = true;
    if (this.phase === "done" || this.phase === "cancelled") return new ArkDoneStep();
    if (this.phase === "idle") {
      this.phase = "starting";
      this.pending = true;
      try {
        const started: ArkValue | __ArkDetachedMarker = await this.session.guardPromise(this.spec.start(), this.session.generation);
        if (started === __DETACHED || this.detached) { this.phase = "cancelled"; return new ArkDoneStep(); }
        this.handle = started as ArkValue;
        if (this.cancelRequested) {
          const error: UniffiError = await this.abort(new UniffiError("UniffiStreamCancelled", "stream was cancelled during start"));
          return new ArkErrorStep(error);
        }
        this.phase = "active";
      } catch (error) {
        this.phase = "done";
        this.finish();
        return new ArkErrorStep(error instanceof UniffiError ? error : new UniffiError("UniffiStreamStart", "stream start failed"));
      } finally { this.pending = false; }
    }
    if (this.handle === null) {
      const error: UniffiError = await this.abort(new UniffiError("UniffiStreamHandle", "stream handle is missing"));
      return new ArkErrorStep(error);
    }
    this.pending = true;
    try {
      const rawValue: ArkCallResult | __ArkDetachedMarker = await this.session.guardPromise(this.spec.next(this.handle), this.session.generation);
      if (rawValue === __DETACHED || this.detached) return new ArkDoneStep();
      const raw: ArkCallResult = rawValue as ArkCallResult;
      const value: ArkValue = __decodeResult(raw);
      if (!(value instanceof ArkRecord)) {
        const error: UniffiError = await this.abort(new UniffiError("UniffiStreamProtocol", "stream step is not tagged"));
        return new ArkErrorStep(error);
      }
      const kind: ArkValue = value.get("kind");
      if (kind === "item") return new ArkItemStep<T>(this.spec.lift(value.get("value")));
      if (kind === "done") { this.phase = "done"; this.finish(); return new ArkDoneStep(); }
      if (kind === "error") {
        this.phase = "done";
        this.finish();
        const errorValue: ArkValue = value.get("error");
        return new ArkErrorStep(this.spec.error(errorValue));
      }
      const error: UniffiError = await this.abort(new UniffiError("UniffiStreamProtocol", "invalid stream step tag"));
      return new ArkErrorStep(error);
    } catch (error) {
      const reason: UniffiError = error instanceof UniffiError ? error : new UniffiError("UniffiStreamError", "stream pull failed");
      const aborted: UniffiError = await this.abort(reason);
      return new ArkErrorStep(aborted);
    } finally { this.pending = false; }
  }
  async cancel(): Promise<void> {
    if (this.phase === "done" || this.phase === "cancelled") return;
    this.cancelRequested = true;
    if (this.phase === "starting") {
      this.phase = "cancelled";
      return;
    }
    if (this.handle === null) {
      this.phase = "cancelled";
      this.finish();
      return;
    }
    this.phase = "cancelled";
    if (!this.cancelIssued) {
      this.cancelIssued = true;
      try {
      const result: void | __ArkDetachedMarker = await this.session.guardPromise(this.spec.cancel(this.handle), this.session.generation);
        void result;
      } finally { this.finish(); }
    } else this.finish();
  }
  detach(): void { this.detached = true; this.phase = "cancelled"; this.pending = false; }
}
interface ArkObjectState {
  readonly session: BackendSession;
  readonly ownerTypeId: number;
  readonly handle: ArkValue;
  disposed: boolean;
}
const __objectStates: Map<ObjectLease, ArkObjectState> = new Map<ObjectLease, ArkObjectState>();
class ArkObjectToken {
  readonly session: BackendSession;
  readonly ownerTypeId: number;
  constructor(session: BackendSession, ownerTypeId: number) { this.session = session; this.ownerTypeId = ownerTypeId; }
}
export interface ArkObjectFactory { readonly ownerTypeId: number; readonly create: (session: BackendSession, handle: ArkValue) => ObjectLease; }
export class ObjectLease {
  protected constructor(session: BackendSession, handle: ArkValue, ownerTypeId: number) {
    __objectStates.set(this, { session, ownerTypeId, handle, disposed: false });
    session.trackObject(this);
  }
  protected objectHandle(): ArkValue {
    const state: ArkObjectState | undefined = __objectStates.get(this);
    if (state === undefined || state.disposed) throw new UniffiError("UniffiUseAfterDispose", "object wrapper was disposed");
    return state.handle;
  }
  protected objectSession(): BackendSession {
    const state: ArkObjectState | undefined = __objectStates.get(this);
    if (state === undefined) throw new UniffiError("UniffiUseAfterDispose", "object wrapper was disposed");
    return state.session;
  }
  assertSession(expected: BackendSession): void {
    const state: ArkObjectState | undefined = __objectStates.get(this);
    if (state === undefined || state.disposed) throw new UniffiError("UniffiUseAfterDispose", "object wrapper was disposed");
    if (expected !== state.session) throw new UniffiError("UniffiObjectSession", "object belongs to another backend session");
  }
  dispose(): void {
    const state: ArkObjectState | undefined = __objectStates.get(this);
    if (state === undefined || state.disposed) return;
    state.disposed = true;
    state.session.releaseObject(state.handle);
    state.session.untrackObject(this);
    __objectStates.delete(this);
  }
  isDisposed(): boolean {
    const state: ArkObjectState | undefined = __objectStates.get(this);
    return state === undefined || state.disposed;
  }
}
function __objectHandle(value: ObjectLease, session: BackendSession, ownerTypeId: number): ArkValue {
  value.assertSession(session);
  const state: ArkObjectState | undefined = __objectStates.get(value);
  if (state === undefined || state.ownerTypeId !== ownerTypeId) throw new UniffiError("UniffiObjectOwner", "object wrapper belongs to another owner type");
  return state.handle;
}
function __decodeResult(raw: ArkCallResult): ArkValue {
  if (raw.kind === "error") throw UniffiError.fromFailure(raw.error);
  return raw.value;
}
function __singleArgument(value: ArkValue): Array<ArkValue> { const result: Array<ArkValue> = new Array<ArkValue>(); result.push(value); return result; }
function __arkLowerBool(value: boolean): ArkValue { if (value !== true && value !== false) throw new UniffiError("UniffiBooleanType", "boolean required"); return value; }
function __arkLiftBool(value: ArkValue): boolean { return __arkLowerBool(value as boolean) as boolean; }
function __arkLowerString(value: string): ArkValue { return value; }
function __arkLiftString(value: ArkValue): string { return __arkLowerString(value as string) as string; }
function __arkLowerBytes(value: Uint8Array): ArkValue { if (!(value instanceof Uint8Array)) throw new UniffiError("UniffiBytesType", "Uint8Array required"); return value; }
function __arkLiftBytes(value: ArkValue): Uint8Array { return __arkLowerBytes(value as Uint8Array) as Uint8Array; }
function __arkLowerI64(value: bigint): ArkValue { return value; }
function __arkLiftI64(value: ArkValue): bigint { return __arkLowerI64(value as bigint) as bigint; }
function __arkLowerU64(value: bigint): ArkValue { if (value < 0n || value > (1n << 64n) - 1n) throw new UniffiError("UniffiNumericError", "U64 is out of range"); return value; }
function __arkLiftU64(value: ArkValue): bigint { return __arkLowerU64(value as bigint) as bigint; }
function __arkNumber(value: number, low: number, high: number, integer: boolean, name: string): ArkValue { if (integer && !Number.isInteger(value)) throw new UniffiError("UniffiNumericError", name + " requires an integer"); if (value < low || value > high || !Number.isFinite(value)) throw new UniffiError("UniffiNumericError", name + " is out of range"); return value; }
function __arkLowerI8(value: number): ArkValue { return __arkNumber(value, -128, 127, true, "I8"); }
function __arkLiftI8(value: ArkValue): number { return __arkNumber(value as number, -128, 127, true, "I8") as number; }
function __arkLowerU8(value: number): ArkValue { return __arkNumber(value, 0, 255, true, "U8"); }
function __arkLiftU8(value: ArkValue): number { return __arkNumber(value as number, 0, 255, true, "U8") as number; }
function __arkLowerI16(value: number): ArkValue { return __arkNumber(value, -32768, 32767, true, "I16"); }
function __arkLiftI16(value: ArkValue): number { return __arkNumber(value as number, -32768, 32767, true, "I16") as number; }
function __arkLowerU16(value: number): ArkValue { return __arkNumber(value, 0, 65535, true, "U16"); }
function __arkLiftU16(value: ArkValue): number { return __arkNumber(value as number, 0, 65535, true, "U16") as number; }
function __arkLowerI32(value: number): ArkValue { return __arkNumber(value, -2147483648, 2147483647, true, "I32"); }
function __arkLiftI32(value: ArkValue): number { return __arkNumber(value as number, -2147483648, 2147483647, true, "I32") as number; }
function __arkLowerU32(value: number): ArkValue { return __arkNumber(value, 0, 4294967295, true, "U32"); }
function __arkLiftU32(value: ArkValue): number { return __arkNumber(value as number, 0, 4294967295, true, "U32") as number; }
function __arkLowerF32(value: number): ArkValue { return __arkNumber(value, -3.4028235e38, 3.4028235e38, false, "F32"); }
function __arkLiftF32(value: ArkValue): number { return __arkNumber(value as number, -3.4028235e38, 3.4028235e38, false, "F32") as number; }
function __arkLowerF64(value: number): ArkValue { if (!Number.isFinite(value)) throw new UniffiError("UniffiNumericError", "F64 requires a finite number"); return value; }
function __arkLiftF64(value: ArkValue): number { return __arkLowerF64(value as number) as number; }
function __arkLowerTimestamp(value: Date): ArkValue { if (!(value instanceof Date) || Number.isNaN(value.getTime())) throw new UniffiError("UniffiTimestampType", "timestamp requires a valid Date"); return value; }
function __arkLiftTimestamp(value: ArkValue): Date { return __arkLowerTimestamp(value as Date) as Date; }
function __arkLowerDuration(value: number): ArkValue { if (!Number.isFinite(value) || value < 0) throw new UniffiError("UniffiDurationType", "duration requires a finite non-negative number"); return value; }
function __arkLiftDuration(value: ArkValue): number { return __arkLowerDuration(value as number) as number; }
function __arkLiftVoid(_value: ArkValue): void { return; }
function __arkLiftStreamError(value: ArkValue): UniffiError { return value instanceof UniffiError ? value : new UniffiError("UniffiStreamError", "stream item failed", null, value); }
export class ArkCallbackRegistration {
  readonly callbackType: number;
  readonly callbackId: number;
  constructor(callbackType: number, callbackId: number) { this.callbackType = callbackType; this.callbackId = callbackId; }
}
export class ArkCallbackFrame { readonly registrations: Array<ArkCallbackRegistration> = new Array<ArkCallbackRegistration>(); }
export class BackendSession {
  readonly backend: ArkBackend;
  readonly host: Host;
  readonly callbacks: CallbackRegistry;
  private readonly inputRegistry: ArkInputRegistry = new ArkInputRegistry();
  private readonly factories: Map<number, ArkObjectFactory> = new Map<number, ArkObjectFactory>();
  private readonly objects: Array<ObjectLease> = [];
  private readonly streams: Array<UniFfiStream<ArkValue>> = [];
  private readonly frames: Array<ArkCallbackFrame> = [];
  private phase: "open" | "closing" | "closed" = "open";
  private detached: boolean = false;
  private closePromise: Promise<void> | null = null;
  private deadlineTimer: number | null = null;
  private closeResolve: (() => void) | null = null;
  private closeReject: ((reason?: Error) => void) | null = null;
  private readonly waiters: Set<() => void> = new Set<() => void>();
  private readonly idleWaiters: Set<() => void> = new Set<() => void>();
  private activeGuards: number = 0;
  private currentGeneration: number = 1;
  constructor(backend: ArkBackend, host: Host = new Host()) {
    this.backend = backend;
    this.host = host;
    this.callbacks = new CallbackRegistry(this);
    host.attachRegistry(this.callbacks, this);
    host.attachInputRegistry(this.inputRegistry, this);
    __CLOSE_POLICY_STATES.set(this, { policy: null, installed: false, open: true });
  }
  get generation(): number { return this.currentGeneration; }
  isDetached(): boolean { return this.detached || this.phase === "closed"; }
  assertCallbackOpen(): void { if (this.phase !== "open") throw new UniffiError("UniffiSessionClosed", "backend session is closed"); }
  private ensureOpen(): void { if (this.phase !== "open") throw new UniffiError("UniffiSessionClosed", "backend session is closed"); }
  private generationActive(generation: number): boolean { return generation === this.currentGeneration && !this.detached && this.phase !== "closed"; }
  guardPromise<T>(promise: Promise<T> | T, generation: number = this.currentGeneration): Promise<T | __ArkDetachedMarker> {
    let settle: () => void;
    this.activeGuards += 1;
    const guarded: Promise<T | __ArkDetachedMarker> = new Promise<T | __ArkDetachedMarker>((resolve, reject) => {
      settle = (): void => { if (!this.waiters.delete(settle)) return; this.activeGuards -= 1; this.notifyIdle(); resolve(__DETACHED); };
      this.waiters.add(settle);
      Promise.resolve(promise).then((value: T): void => { if (!this.waiters.delete(settle)) return; this.activeGuards -= 1; this.notifyIdle(); resolve(this.generationActive(generation) ? value : __DETACHED); }, (error: Error): void => { if (!this.waiters.delete(settle)) return; this.activeGuards -= 1; this.notifyIdle(); if (this.generationActive(generation)) reject(error); else resolve(__DETACHED); });
    });
    guarded.catch((): void => undefined);
    return guarded;
  }
  private notifyIdle(): void { if (this.activeGuards !== 0) return; for (const resolve of Array.from(this.idleWaiters)) { this.idleWaiters.delete(resolve); resolve(); } }
  private awaitIdle(): Promise<void> { if (this.activeGuards === 0) return Promise.resolve(); return new Promise<void>((resolve): void => { this.idleWaiters.add(resolve); }); }
  invokeSync(operationId: number, args: Array<ArkValue>): ArkValue { this.ensureOpen(); return __decodeResult(this.backend.invokeSync(operationId, args)); }
  invokeAsync(operationId: number, args: Array<ArkValue>): Promise<ArkValue> {
    try { this.ensureOpen(); } catch (error) { return Promise.reject(error); }
    const generation: number = this.currentGeneration;
    const call: Promise<ArkValue> = Promise.resolve().then(() => this.backend.invokeAsync(operationId, args)).then((raw: ArkCallResult): ArkValue => __decodeResult(raw));
    const exposed: Promise<ArkValue> = this.guardPromise(call, generation).then((raw: ArkValue | __ArkDetachedMarker): ArkValue => { if (raw === __DETACHED) throw new UniffiError("UniffiSessionClosed", "backend session is closed"); return raw as ArkValue; });
    exposed.catch((): void => undefined);
    return exposed;
  }
  registerCallback(callbackType: number, callback: ArkCallbackAdapter, contract: ArkCallbackContract): number { this.ensureOpen(); const id: number = this.callbacks.register(callbackType, callback, contract); const frame: ArkCallbackFrame | undefined = this.frames[this.frames.length - 1]; if (frame !== undefined && contract.retention === "scoped") { this.callbacks.frameRetain(callbackType, id); frame.registrations.push(new ArkCallbackRegistration(callbackType, id)); } return id; }
  retainCallback(callbackType: number, callbackId: number): ArkCallbackLease { return this.callbacks.retain(callbackType, callbackId); }
  releaseCallback(callbackType: number, callbackId: number): void { this.callbacks.release(callbackType, callbackId); }
  beginCallFrame(): ArkCallbackFrame { const frame: ArkCallbackFrame = new ArkCallbackFrame(); this.frames.push(frame); return frame; }
  endCallFrame(frame: ArkCallbackFrame): void { const index: number = this.frames.indexOf(frame); if (index >= 0) this.frames.splice(index, 1); for (const registration of frame.registrations) this.callbacks.frameRelease(registration.callbackType, registration.callbackId); }
  trackObject(object: ObjectLease): void { this.objects.push(object); }
  untrackObject(object: ObjectLease): void { const index: number = this.objects.indexOf(object); if (index >= 0) this.objects.splice(index, 1); }
  releaseObject(handle: ArkValue): void { if (this.isDetached()) return; try { this.backend.releaseObject(handle); } catch (_) {} }
  registerObjectFactory(typeId: number, factory: ArkObjectFactory): void {
    const existing: ArkObjectFactory | undefined = this.factories.get(typeId);
    if (existing === undefined) this.factories.set(typeId, factory);
    else if (existing.ownerTypeId !== factory.ownerTypeId) throw new UniffiError("UniffiObjectFactory", "owner type ID has multiple factories");
  }
  liftObject<T extends ObjectLease>(typeId: number, handle: ArkValue): T { const factory: ArkObjectFactory | undefined = this.factories.get(typeId); if (factory === undefined) throw new UniffiError("UniffiObjectFactory", "missing owner object factory"); if (factory.ownerTypeId !== typeId) throw new UniffiError("UniffiObjectFactory", "owner factory type mismatch"); return factory.create(this, handle) as T; }
  createInputStream<T>(source: UniFfiInputStream<T>, lower: (value: T) => ArkValue): number { this.ensureOpen(); return this.inputRegistry.register(source, lower); }
  createInputView<T>(handle: ArkValue, lift: (value: ArkValue) => T): UniFfiInputStream<T> { return new ArkInputView<T>(this, handle, lift); }
  pullInputStream(handle: ArkValue): Promise<ArkStreamStep<ArkValue>> {
    if (this.isDetached()) return Promise.resolve(new ArkDoneStep());
    const generation: number = this.currentGeneration;
    const guarded: Promise<ArkStreamStep<ArkValue> | __ArkDetachedMarker> = this.guardPromise(this.host.pullInputStream(handle), generation);
    const result: Promise<ArkStreamStep<ArkValue>> = guarded.then((step: ArkStreamStep<ArkValue> | __ArkDetachedMarker): ArkStreamStep<ArkValue> => step === __DETACHED ? new ArkDoneStep() : step as ArkStreamStep<ArkValue>);
    result.catch((): void => undefined);
    return result;
  }
  async cancelInputStream(handle: ArkValue): Promise<void> { if (this.isDetached()) return; try { const result: void | __ArkDetachedMarker = await this.guardPromise(this.host.cancelInputStream(handle), this.currentGeneration); void result; } catch (_) {} }
  releaseInputStream(handle: ArkValue): void { if (this.isDetached()) return; this.host.releaseInputStream(handle); }
  releaseOutputStream(handle: ArkValue): void { if (this.isDetached()) return; try { this.backend.releaseOutputStream(handle); } catch (_) {} }
  createOutputStream<T>(spec: ArkOutputSpec<T>): UniFfiStream<T> { this.ensureOpen(); const stream: ArkOutputStream<T> = new ArkOutputStream<T>(this, spec); this.streams.push(stream as UniFfiStream<ArkValue>); return stream; }
  close(): Promise<void> {
    if (this.closePromise !== null) return this.closePromise;
    if (this.phase === "closed") return Promise.resolve();
    let policy: ClosePolicy;
    try { policy = __sessionClosePolicy(this); } catch (error) {
      const rejected: Promise<void> = Promise.reject(error);
      rejected.catch((): void => undefined);
      this.closePromise = rejected;
      this.phase = "closed";
      this.detached = true;
      this.currentGeneration += 1;
      __markPolicyClosed(this);
      for (const stream of this.streams) { (stream as ArkOutputStream<ArkValue>).detach?.(); }
      this.streams.splice(0);
      this.inputRegistry.detach();
      for (const object of this.objects.slice()) { try { object.dispose(); } catch (_) {} }
      this.objects.splice(0);
      this.callbacks.detach();
      this.frames.splice(0);
      return rejected;
    }
    this.phase = "closing";
    __markPolicyClosed(this);
    this.closePromise = new Promise<void>((resolve, reject): void => { this.closeResolve = resolve; this.closeReject = reject; });
    this.deadlineTimer = setTimeout((): void => this.deadlineDetach(), policy.graceMs);
    this.runTeardown().then((errors: Array<Error>): void => this.finishNaturalClose(errors), (error: Error): void => this.finishNaturalClose([error])).catch((): void => undefined);
    return this.closePromise;
  }
  private async runTeardown(): Promise<Array<Error>> {
    const errors: Array<Error> = [];
    const pending: Array<Promise<void>> = [];
    for (const stream of this.streams.slice()) {
      try {
        const result: Promise<void> | void = stream.cancel();
        pending.push(this.guardPromise(result, this.currentGeneration).then((): void => undefined).catch((error: Error): void => { if (!this.detached) errors.push(error); }));
      } catch (error) { if (!this.detached) errors.push(error); }
    }
    try {
      const inputClose: Promise<void> = this.inputRegistry.close();
      pending.push(this.guardPromise(inputClose, this.currentGeneration).then((): void => undefined).catch((error: Error): void => { if (!this.detached) errors.push(error); }));
    } catch (error) { if (!this.detached) errors.push(error); }
    for (const object of this.objects.slice()) { try { object.dispose(); } catch (_) {} }
    if (this.backend.close !== undefined) {
      try {
        const backendClose: Promise<void> | void = this.backend.close();
        pending.push(this.guardPromise(backendClose, this.currentGeneration).then((): void => undefined).catch((error: Error): void => { if (!this.detached) errors.push(error); }));
      } catch (error) { if (!this.detached) errors.push(error); }
    }
    await Promise.all(pending);
    if (!this.detached) await this.awaitIdle();
    return errors;
  }
  private finishNaturalClose(errors: Array<Error>): void {
    if (this.phase === "closed") return;
    if (this.deadlineTimer !== null) { clearTimeout(this.deadlineTimer); this.deadlineTimer = null; }
    if (!this.detached) { this.callbacks.detach(); this.phase = "closed"; this.currentGeneration += 1; }
    const resolve: (() => void) | null = this.closeResolve; const reject: ((reason?: Error) => void) | null = this.closeReject; this.closeResolve = null; this.closeReject = null;
    if (this.detached || errors.length === 0) resolve?.(); else reject?.(errors[0]);
  }
  private deadlineDetach(): void {
    if (this.phase === "closed" || this.detached) return;
    this.detached = true; this.phase = "closed"; this.currentGeneration += 1;
    if (this.deadlineTimer !== null) { clearTimeout(this.deadlineTimer); this.deadlineTimer = null; }
    for (const settle of Array.from(this.waiters)) settle();
    for (const resolve of Array.from(this.idleWaiters)) { this.idleWaiters.delete(resolve); resolve(); }
    for (const stream of this.streams) { (stream as ArkOutputStream<ArkValue>).detach?.(); }
    this.streams.splice(0);
    this.inputRegistry.detach();
    for (const object of this.objects.slice()) { try { object.dispose(); } catch (_) {} }
    this.objects.splice(0);
    this.callbacks.detach();
    this.frames.splice(0);
    const resolve: (() => void) | null = this.closeResolve; this.closeResolve = null; this.closeReject = null; resolve?.();
  }
}
export function createBackendSession(backend: ArkBackend, host: Host = new Host()): BackendSession { return new BackendSession(backend, host); }
"#;

fn render_runtime_declarations() -> String {
    // Keep this declaration text in lock-step with ARK_RUNTIME's public names.
    r#"export type ArkPrimitive = string | number | boolean | bigint;
export declare class ArkRecord { set(name: string, value: ArkValue): void; has(name: string): boolean; get(name: string): ArkValue; }
export type ArkValue = ArkPrimitive | Uint8Array | Date | ArkRecord | Array<ArkValue> | Map<ArkValue, ArkValue> | Set<ArkValue> | null;
export interface ArkFailure { readonly errorName: string; readonly message: string; readonly variant: string | null; readonly data: ArkValue | null; }
export declare class ArkValueResult { readonly kind: "value"; readonly value: ArkValue; constructor(value: ArkValue); }
export declare class ArkErrorResult { readonly kind: "error"; readonly error: ArkFailure; constructor(error: ArkFailure); }
export type ArkCallResult = ArkValueResult | ArkErrorResult;
export interface ArkBackend { invokeSync(operationId: number, args: Array<ArkValue>): ArkCallResult; invokeAsync(operationId: number, args: Array<ArkValue>): Promise<ArkCallResult>; releaseObject(handle: ArkValue): void; releaseOutputStream(handle: ArkValue): void; close?(): Promise<void> | void; }
export declare class UniffiError extends Error { readonly errorName: string; readonly variant: string | null; readonly data: ArkValue | null; constructor(errorName?: string, message?: string, variant?: string | null, data?: ArkValue | null); }
export declare class ArkItemStep<T> { readonly kind: "item"; readonly value: T; constructor(value: T); }
export declare class ArkDoneStep { readonly kind: "done"; }
export declare class ArkErrorStep { readonly kind: "error"; readonly error: UniffiError; constructor(error: UniffiError); }
export type ArkStreamStep<T> = ArkItemStep<T> | ArkDoneStep | ArkErrorStep;
export interface UniFfiStream<T> { next(): Promise<ArkStreamStep<T>>; cancel(): Promise<void>; }
export interface UniFfiInputStream<T> { next(): Promise<ArkStreamStep<T>>; cancel(): Promise<void>; release(): void; }
export interface ArkCallbackAdapter { invokeSync(methodId: number, args: Array<ArkValue>): ArkValue; invokeAsync(methodId: number, invocationId: number, args: Array<ArkValue>): Promise<ArkValue>; }
export interface ArkCallbackContract { readonly retention: "scoped" | "retained"; readonly threading: "callingThread" | "mayCrossThread"; readonly reentrancy: "forbidden" | "allowed"; }
export declare class ArkCallbackLease { release(): void; }
export declare class CallbackRegistry { register(callbackType: number, callback: ArkCallbackAdapter, contract: ArkCallbackContract): number; retain(callbackType: number, callbackId: number): ArkCallbackLease; release(callbackType: number, callbackId: number): void; invokeSync(callbackType: number, callbackId: number, methodId: number, args: Array<ArkValue>): ArkValue; invokeAsync(callbackType: number, callbackId: number, methodId: number, invocationId: number, args: Array<ArkValue>): Promise<ArkValue>; }
export declare class Host { retainCallback(callbackType: number, callbackId: number): void; releaseCallback(callbackType: number, callbackId: number): void; invokeCallbackSync(callbackType: number, callbackId: number, methodId: number, args?: Array<ArkValue>): ArkValue; invokeCallbackAsync(callbackType: number, callbackId: number, methodId: number, invocationId: number, args?: Array<ArkValue>): Promise<ArkValue>; invokeCallbackSyncResult(callbackType: number, callbackId: number, methodId: number, args?: Array<ArkValue>): ArkCallbackResult; invokeCallbackAsyncResult(callbackType: number, callbackId: number, methodId: number, invocationId: number, args?: Array<ArkValue>): Promise<ArkCallbackResult>; pullInputStream(handle: ArkValue): Promise<ArkStreamStep<ArkValue>>; cancelInputStream(handle: ArkValue): Promise<void>; releaseInputStream(handle: ArkValue): void; }
export declare class ArkCallbackRegistration { readonly callbackType: number; readonly callbackId: number; }
export declare class ArkCallbackFrame { readonly registrations: Array<ArkCallbackRegistration>; }
export declare class BackendSession { readonly backend: ArkBackend; readonly host: Host; readonly callbacks: CallbackRegistry; constructor(backend: ArkBackend, host?: Host); invokeSync(operationId: number, args: Array<ArkValue>): ArkValue; invokeAsync(operationId: number, args: Array<ArkValue>): Promise<ArkValue>; registerCallback(callbackType: number, callback: ArkCallbackAdapter, contract: ArkCallbackContract): number; retainCallback(callbackType: number, callbackId: number): ArkCallbackLease; releaseCallback(callbackType: number, callbackId: number): void; beginCallFrame(): ArkCallbackFrame; endCallFrame(frame: ArkCallbackFrame): void; createInputStream<T>(source: UniFfiInputStream<T>, lower: (value: T) => ArkValue): number; createInputView<T>(handle: ArkValue, lift: (value: ArkValue) => T): UniFfiInputStream<T>; pullInputStream(handle: ArkValue): Promise<ArkStreamStep<ArkValue>>; cancelInputStream(handle: ArkValue): Promise<void>; releaseInputStream(handle: ArkValue): void; releaseOutputStream(handle: ArkValue): void; close(): Promise<void>; }
export declare class ObjectLease { dispose(): void; isDisposed(): boolean; }
export function createBackendSession(backend: ArkBackend, host?: Host): BackendSession;
"#
    .to_owned()
}

fn render_callback_interface_adapter(
    model: &Model<'_>,
    ty: &AstType,
    helpers: &mut Helpers,
) -> Result<String, FacadeError> {
    let name = model.type_name(&ty.source_key);
    let adapter_name = format!("__ArkCallbackAdapter_{}", ty.id.index());
    let methods = model
        .operations
        .iter()
        .copied()
        .filter(|operation| {
            matches!(operation.source_key.owner(), OperationOwner::Callback(key) if key == &ty.source_key && operation.kind == OperationKind::CallbackMethod)
        })
        .collect::<Vec<_>>();
    let mut out = String::new();
    for operation in &methods {
        if let Some(error_key) = &operation.throws {
            out.push_str(&render_callback_error_helper(
                model, operation, error_key, helpers,
            ));
        }
    }
    out.push_str(&format!(
        "function __makeCallbackAdapter{ty_id}(callback: {name}, session: BackendSession): ArkCallbackAdapter {{\n",
        ty_id = ty.id.index()
    ));
    out.push_str("  return new ");
    out.push_str(&adapter_name);
    out.push_str("(callback, session);\n}\n");
    out.push_str(&format!(
        "class {adapter_name} implements ArkCallbackAdapter {{\n  private readonly callback: {name};\n  private readonly session: BackendSession;\n  constructor(callback: {name}, session: BackendSession) {{ this.callback = callback; this.session = session; }}\n"
    ));
    out.push_str("  invokeSync(methodId: number, args: Array<ArkValue>): ArkValue {\n    switch (methodId) {\n");
    for operation in &methods {
        let method_id = operation
            .callback_method_id
            .ok_or(FacadeError::MissingCallbackMethodId { id: operation.id })?;
        if operation.async_kind == AsyncKind::Async {
            out.push_str(&format!("      case {}: throw new UniffiError(\"UniffiCallbackProtocol\", \"async callback method invoked synchronously\");\n", method_id));
            continue;
        }
        let mut lower_args = Vec::new();
        for (index, argument) in operation.arguments.iter().enumerate() {
            let helper_name = lift_helper(
                model,
                helpers,
                &format!(
                    "callback_{}_{}_arg_{index}",
                    ty.id.index(),
                    operation.id.index()
                ),
                &argument.ty,
                &format!("argument[{index}]"),
                Some(operation.id),
            );
            lower_args.push(format!("{}(args[{index}], this.session)", helper_name));
        }
        let return_call = if let Some(return_type) = &operation.return_type {
            let lower_name = lower_helper(
                model,
                helpers,
                &format!("callback_{}_{}_return", ty.id.index(), operation.id.index()),
                return_type,
                "callback.return",
                Some(operation.id),
            );
            let call = format!(
                "this.callback.{}({})",
                operation.name,
                lower_args.join(", ")
            );
            if operation.throws.is_some() {
                format!("try {{ return {}({}, this.session); }} catch (error) {{ throw __arkLowerCallbackError{}(error, this.session); }}", lower_name, call, operation.id.index())
            } else {
                format!("return {}({}, this.session);", lower_name, call)
            }
        } else {
            let call = format!(
                "this.callback.{}({})",
                operation.name,
                lower_args.join(", ")
            );
            if operation.throws.is_some() {
                format!("try {{ {}; return null; }} catch (error) {{ throw __arkLowerCallbackError{}(error, this.session); }}", call, operation.id.index())
            } else {
                format!("{}; return null;", call)
            }
        };
        out.push_str(&format!("      case {}: {}\n", method_id, return_call));
    }
    out.push_str("      default: throw new UniffiError(\"UniffiCallbackMethod\", \"unrecognized callback method ID\");\n    }\n  }\n");
    out.push_str("  async invokeAsync(methodId: number, invocationId: number, args: Array<ArkValue>): Promise<ArkValue> {\n    switch (methodId) {\n");
    for operation in &methods {
        let method_id = operation
            .callback_method_id
            .ok_or(FacadeError::MissingCallbackMethodId { id: operation.id })?;
        if operation.async_kind != AsyncKind::Async {
            out.push_str(&format!(
                "      case {}: return Promise.reject(new UniffiError(\"UniffiCallbackProtocol\", \"sync callback method invoked asynchronously\"));\n",
                method_id
            ));
            continue;
        }
        let mut lower_args = Vec::new();
        for (index, argument) in operation.arguments.iter().enumerate() {
            let helper_name = lift_helper(
                model,
                helpers,
                &format!(
                    "callback_async_{}_{}_arg_{index}",
                    ty.id.index(),
                    operation.id.index()
                ),
                &argument.ty,
                &format!("argument[{index}]"),
                Some(operation.id),
            );
            lower_args.push(format!("{}(args[{index}], this.session)", helper_name));
        }
        if let Some(return_type) = &operation.return_type {
            let lower_name = lower_helper(
                model,
                helpers,
                &format!(
                    "callback_async_{}_{}_return",
                    ty.id.index(),
                    operation.id.index()
                ),
                return_type,
                "callback.return",
                Some(operation.id),
            );
            let call = if operation.async_kind == AsyncKind::Async {
                format!(
                    "await this.callback.{}({})",
                    operation.name,
                    lower_args.join(", ")
                )
            } else {
                format!(
                    "this.callback.{}({})",
                    operation.name,
                    lower_args.join(", ")
                )
            };
            if operation.throws.is_some() {
                out.push_str(&format!(
                    "      case {}: try {{ return {}({}, this.session); }} catch (error) {{ throw __arkLowerCallbackError{}(error, this.session); }}\n",
                    method_id, lower_name, call, operation.id.index()
                ));
            } else {
                out.push_str(&format!(
                    "      case {}: return {}({}, this.session);\n",
                    method_id, lower_name, call
                ));
            }
        } else {
            let call = if operation.async_kind == AsyncKind::Async {
                format!(
                    "await this.callback.{}({})",
                    operation.name,
                    lower_args.join(", ")
                )
            } else {
                format!(
                    "this.callback.{}({})",
                    operation.name,
                    lower_args.join(", ")
                )
            };
            if operation.throws.is_some() {
                out.push_str(&format!(
                    "      case {}: try {{ {}; return null; }} catch (error) {{ throw __arkLowerCallbackError{}(error, this.session); }}\n",
                    method_id, call, operation.id.index()
                ));
            } else {
                out.push_str(&format!(
                    "      case {}: {}; return null;\n",
                    method_id, call
                ));
            }
        }
    }
    out.push_str("      default: return Promise.reject(new UniffiError(\"UniffiCallbackMethod\", \"unrecognized callback method ID\"));\n    }\n  }\n}\n");
    Ok(out)
}

fn render_callback_error_helper(
    model: &Model<'_>,
    operation: &AstOperation,
    error_key: &TypeSourceKey,
    helpers: &mut Helpers,
) -> String {
    let Some(error_type) = model.ty_for(error_key) else {
        return format!(
            "function __arkLowerCallbackError{}(error: Error, _session: BackendSession): UniffiError {{ return error instanceof UniffiError ? error : new UniffiError(\"UniffiCallbackError\", \"callback error is not declared\"); }}\n",
            operation.id.index()
        );
    };
    let error_name = model.type_name(error_key);
    let mut out = format!(
        "function __arkLowerCallbackError{}(error: Error, session: BackendSession): UniffiError {{\n  if (!(error instanceof {})) {{ if (error instanceof UniffiError && error.errorName === \"{}\") return new {}(error.message, error.variant, error.data); return new UniffiError(\"{}\", \"callback failed\"); }}\n  const declared: {} = error as {};\n  const variant: string | null = declared.variant;\n",
        operation.id.index(),
        error_name,
        escape_string(&error_name),
        error_name,
        escape_string(&error_name),
        error_name,
        error_name
    );
    if let AstTypeKind::Error { variants } = &error_type.kind {
        for variant in variants {
            if variant.fields.is_empty() {
                out.push_str(&format!(
                    "  if (variant === \"{}\") return new {}(declared.message, variant, null);\n",
                    variant.name, error_name
                ));
                continue;
            }
            out.push_str(&format!(
                "  if (variant === \"{}\") {{ if (declared.data === null) throw new UniffiError(\"UniffiCallbackProtocol\", \"callback error payload is missing\"); const payloadValue: ArkValue = declared.data; if (!(payloadValue instanceof ArkRecord)) throw new UniffiError(\"UniffiCallbackProtocol\", \"callback error payload must be an ArkRecord\"); const payload: ArkRecord = payloadValue; const data: ArkRecord = new ArkRecord();\n",
                variant.name
            ));
            for field in &variant.fields {
                let lower = lower_helper(
                    model,
                    helpers,
                    &format!(
                        "callback_error_{}_{}_{}",
                        operation.id.index(),
                        safe_ident(&variant.name),
                        safe_ident(&field.name)
                    ),
                    &field.ty,
                    &format!(
                        "callback.error.variant[{}].field[{}]",
                        variant.name, field.name
                    ),
                    Some(operation.id),
                );
                out.push_str(&format!(
                    "    data.set(\"{}\", {}(payload.get(\"{}\") as {}, session));\n",
                    field.name,
                    lower,
                    field.name,
                    render_type_name(model, &field.ty)
                ));
            }
            out.push_str(&format!(
                "    return new {}(declared.message, variant, data); }}\n",
                error_name
            ));
        }
    }
    out.push_str("  return new UniffiError(\"UniffiCallbackProtocol\", \"unrecognized callback error variant\");\n}\n");
    out
}

fn render_operation_helpers(
    model: &Model<'_>,
    operation: &AstOperation,
    helpers: &mut Helpers,
) -> Result<String, FacadeError> {
    let mut out = String::new();
    let mut lower_names = Vec::new();
    for (index, argument) in operation.arguments.iter().enumerate() {
        lower_names.push(lower_helper(
            model,
            helpers,
            &format!("operation_{}_argument_{index}", operation.id.index()),
            &argument.ty,
            &format!("argument[{index}]"),
            Some(operation.id),
        ));
    }
    let lift_name = operation
        .return_type
        .as_ref()
        .filter(|return_type| !matches!(return_type, ValueType::OutputStream { .. }))
        .map(|return_type| {
            lift_helper(
                model,
                helpers,
                &format!("operation_{}_return", operation.id.index()),
                return_type,
                "return",
                Some(operation.id),
            )
        });

    let receiver_lower_name = match operation.source_key.owner() {
        OperationOwner::Value(key) if operation.kind != OperationKind::Constructor => {
            Some(lower_helper(
                model,
                helpers,
                &format!("operation_{}_receiver", operation.id.index()),
                &ValueType::Named(key.clone()),
                "receiver",
                Some(operation.id),
            ))
        }
        _ => None,
    };

    let receiver_type_name = operation
        .receiver_type
        .and_then(|type_id| model.types.iter().find(|ty| ty.id == type_id))
        .filter(|ty| matches!(ty.kind, AstTypeKind::Object { .. }))
        .map(|ty| model.type_name(&ty.source_key));
    let function_name = if receiver_type_name.is_some() {
        format!("__invokeObject{}", operation.id.index())
    } else if matches!(operation.source_key.owner(), OperationOwner::Value(_)) {
        format!("__invokeValue{}", operation.id.index())
    } else {
        format!("__invokeOperation{}", operation.id.index())
    };
    let mut signature = String::new();
    if let Some(receiver) = &receiver_type_name {
        signature.push_str(&format!(
            "self: {receiver}, session: BackendSession, getHandle: () => ArkValue"
        ));
    } else if matches!(operation.source_key.owner(), OperationOwner::Value(_))
        && operation.kind != OperationKind::Constructor
    {
        let owner = model.type_name(match operation.source_key.owner() {
            OperationOwner::Value(key) => key,
            _ => unreachable!(),
        });
        signature.push_str(&format!("self: {owner}, session: BackendSession"));
    } else {
        signature.push_str("session: BackendSession");
    }
    for argument in &operation.arguments {
        signature.push_str(&format!(
            ", {}{}: {}",
            argument.name,
            if argument.default.is_some() { "?" } else { "" },
            render_type_name(model, &argument.ty)
        ));
    }
    let return_type = operation_return_type(model, operation);
    let normalize_error = operation
        .throws
        .as_ref()
        .and_then(|key| model.ty_for(key))
        .map(|ty| {
            let error_name = model.type_name(&ty.source_key);
            format!(
                "if (error instanceof UniffiError && error.errorName === \"{}\") throw new {}(error.message, error.variant, error.data); ",
                escape_string(&error_name), error_name
            )
        })
        .unwrap_or_default();
    let propagate_error = format!(
        "{}if (error instanceof UniffiError) throw error; throw new UniffiError(\"UniffiOperation\", \"operation failed\"); ",
        normalize_error
    );
    out.push_str(&format!(
        "function {function_name}({signature}): {return_type} {{\n"
    ));
    out.push_str("  const __frame: ArkCallbackFrame = session.beginCallFrame();\n  const __args: Array<ArkValue> = new Array<ArkValue>();\n  let __omitted: boolean = false;\n");
    for (argument, lower_name) in operation.arguments.iter().zip(lower_names.iter()) {
        let value = &argument.name;
        let default = argument
            .default
            .as_ref()
            .map(|default| render_default_for_type(default, &argument.ty));
        if matches!(argument.default, Some(DefaultValue::Unspecified)) {
            out.push_str(&format!(
                "  if ({value} === undefined) {{ __omitted = true; }} else {{ if (__omitted) throw new UniffiError(\"UniffiArgumentCount\", \"argument follows omitted default\"); __args.push({lower_name}({value} as {}, session)); }}\n",
                render_type_name(model, &argument.ty)
            ));
        } else if let Some(default) = default {
            out.push_str(&format!(
                "  if (__omitted) throw new UniffiError(\"UniffiArgumentCount\", \"argument follows omitted default\"); if ({value} === undefined) __args.push({lower_name}({default} as {}, session)); else __args.push({lower_name}({value} as {}, session));\n",
                render_type_name(model, &argument.ty),
                render_type_name(model, &argument.ty)
            ));
        } else {
            out.push_str(&format!(
                "  if (__omitted || {value} === undefined) throw new UniffiError(\"UniffiUndefined\", \"undefined is not a UniFFI value\"); __args.push({lower_name}({value} as {}, session));\n",
                render_type_name(model, &argument.ty)
            ));
        }
    }
    if receiver_type_name.is_some() {
        out.push_str("  self.assertSession(session);\n  __args.unshift(getHandle());\n");
    } else if matches!(operation.source_key.owner(), OperationOwner::Value(_))
        && operation.kind != OperationKind::Constructor
    {
        out.push_str(&format!(
            "  __args.unshift({}(self, session));\n",
            receiver_lower_name
                .as_deref()
                .unwrap_or("__arkLowerInvalidReceiver")
        ));
    }

    if is_output_stream_return(operation) {
        let item = match operation.return_type.as_ref() {
            Some(ValueType::OutputStream { item, .. }) => item.as_ref(),
            _ => unreachable!(),
        };
        let item_lift = lift_helper(
            model,
            helpers,
            &format!("operation_{}_stream_item", operation.id.index()),
            item,
            "return.item",
            Some(operation.id),
        );
        let resource = model.stream_resource(operation, StreamDirection::Output)?;
        let error_decoder = match &resource.error {
            ValueType::Named(key)
                if model
                    .ty_for(key)
                    .is_some_and(|ty| matches!(ty.kind, AstTypeKind::Error { .. })) =>
            {
                let helper = lift_helper(
                    model,
                    helpers,
                    &format!("operation_{}_stream_error", operation.id.index()),
                    &resource.error,
                    "return.error",
                    Some(operation.id),
                );
                format!(
                    "(value: ArkValue): UniffiError => {}(value, session) as UniffiError",
                    helper
                )
            }
            _ => "(value: ArkValue): UniffiError => __arkLiftStreamError(value)".to_owned(),
        };
        let start_id = resource
            .slot_operation_ids
            .get(&OperationKind::OutputStreamStart)
            .copied()
            .ok_or(FacadeError::MissingStreamSlot {
                operation: operation.id,
            })?;
        if start_id != operation.id {
            return Err(FacadeError::MissingStreamSlot {
                operation: operation.id,
            });
        }
        let next_id = resource
            .slot_operation_ids
            .get(&OperationKind::OutputStreamNext)
            .copied()
            .ok_or(FacadeError::MissingStreamSlot {
                operation: operation.id,
            })?;
        let cancel_id = resource
            .slot_operation_ids
            .get(&OperationKind::OutputStreamCancel)
            .copied()
            .ok_or(FacadeError::MissingStreamSlot {
                operation: operation.id,
            })?;
        let cancel = format!("(handle: ArkValue): Promise<void> => session.invokeAsync({}, __singleArgument(handle)).then((): void => undefined)", cancel_id.index());
        let start = if operation.async_kind == AsyncKind::Async {
            format!(
                "start: (): Promise<ArkValue> => session.invokeAsync({}, __args)",
                start_id.index()
            )
        } else {
            format!(
                "start: (): ArkValue => session.invokeSync({}, __args)",
                start_id.index()
            )
        };
        out.push_str(&format!(
            "  const __outputSpec: ArkOutputSpec<{}> = {{ {start}, next: (handle: ArkValue): Promise<ArkCallResult> => session.invokeAsync({}, __singleArgument(handle)).then((value: ArkValue): ArkCallResult => new ArkValueResult(value)), cancel: {cancel}, lift: (value: ArkValue): {} => {item_lift}(value, session), error: {error_decoder}, release: (handle: ArkValue): void => session.releaseOutputStream(handle), onClose: (): void => session.endCallFrame(__frame) }};\n  return session.createOutputStream(__outputSpec);\n",
            render_type_name(model, item),
            next_id.index(),
            render_type_name(model, item)
        ));
        out.push_str("}\n");
        return Ok(out);
    }

    if operation.async_kind == AsyncKind::Async {
        let resolved_return = operation
            .return_type
            .as_ref()
            .map(|value| render_type_name(model, value))
            .unwrap_or_else(|| "void".to_owned());
        out.push_str(&format!(
            "  return session.invokeAsync({}, __args).then((value: ArkValue): {} => {{ session.endCallFrame(__frame); return {}(value, session); }}, (error: Error): {} => {{ session.endCallFrame(__frame); {} }});\n",
            operation.id.index(),
            resolved_return,
            lift_name.as_deref().unwrap_or("__arkLiftVoid"),
            resolved_return,
            propagate_error
        ));
    } else {
        if operation.return_type.is_some() {
            out.push_str(&format!(
                "  try {{ const __value: ArkValue = session.invokeSync({}, __args); const __result: {} = {}(__value, session); session.endCallFrame(__frame); return __result; }} catch (error) {{ session.endCallFrame(__frame); {} }}\n",
                operation.id.index(),
                return_type,
                lift_name.as_deref().unwrap_or("__arkLiftVoid"),
                propagate_error
            ));
        } else {
            out.push_str(&format!(
                "  try {{ session.invokeSync({}, __args); session.endCallFrame(__frame); return; }} catch (error) {{ session.endCallFrame(__frame); {} }}\n",
                operation.id.index(),
                propagate_error
            ));
        }
    }
    out.push_str("}\n");
    Ok(out)
}

fn is_output_stream_return(operation: &AstOperation) -> bool {
    matches!(operation.return_type, Some(ValueType::OutputStream { .. }))
        && operation.kind == OperationKind::OutputStreamStart
}

fn lower_helper(
    model: &Model<'_>,
    helpers: &mut Helpers,
    label: &str,
    ty: &ValueType,
    path: &str,
    operation_id: Option<uniffi_js_abi::OperationId>,
) -> String {
    let helper_name = helpers.next(&format!("lower_{}", safe_ident(label)));
    let parameter_type = render_type_name(model, ty);
    let body = lower_body(model, helpers, ty, "value", path, operation_id);
    helpers.lower.push(format!(
        "function {helper_name}(value: {parameter_type}, session: BackendSession): ArkValue {{\n{body}\n}}"
    ));
    helper_name
}

fn lower_body(
    model: &Model<'_>,
    helpers: &mut Helpers,
    ty: &ValueType,
    expression: &str,
    path: &str,
    operation_id: Option<uniffi_js_abi::OperationId>,
) -> String {
    match ty {
        ValueType::Scalar(scalar) => {
            format!("return __arkLower{}({expression});", scalar_name(*scalar))
        }
        ValueType::Timestamp => format!("return __arkLowerTimestamp({expression});"),
        ValueType::Duration => format!("return __arkLowerDuration({expression});"),
        ValueType::Optional(inner) => {
            let nested = lower_nested_call(model, helpers, inner, expression, path, operation_id);
            format!(
                "if ({expression} === undefined) throw new UniffiError(\"UniffiUndefined\", \"undefined is not a UniFFI value; use null for optional\");\nif ({expression} === null) return null;\nreturn {nested};"
            )
        }
        ValueType::Sequence(inner) => {
            let item_type = render_type_name(model, inner);
            let nested = lower_nested_call(
                model,
                helpers,
                inner,
                "item",
                &format!("{path}.item"),
                operation_id,
            );
            format!(
                "if (!({expression} instanceof Array)) throw new UniffiError(\"UniffiSequenceType\", \"sequence requires Array\");\nconst result: Array<ArkValue> = new Array<ArkValue>();\nfor (const item of {expression}) {{ result.push({nested}); }}\nreturn result; // {item_type}"
            )
        }
        ValueType::Map(key, value) => {
            let key_call = lower_nested_call(
                model,
                helpers,
                key,
                "entryKey",
                &format!("{path}.key"),
                operation_id,
            );
            let value_call = lower_nested_call(
                model,
                helpers,
                value,
                "entryValue",
                &format!("{path}.value"),
                operation_id,
            );
            format!(
                "const result: Map<ArkValue, ArkValue> = new Map<ArkValue, ArkValue>();\n{expression}.forEach((entryValue: {value_type}, entryKey: {key_type}): void => {{ result.set({key_call}, {value_call}); }});\nreturn result;",
                value_type = render_type_name(model, value),
                key_type = render_type_name(model, key)
            )
        }
        ValueType::Set(inner) => {
            let nested = lower_nested_call(
                model,
                helpers,
                inner,
                "item",
                &format!("{path}.set-item"),
                operation_id,
            );
            format!(
                "const result: Set<ArkValue> = new Set<ArkValue>();\n{expression}.forEach((item: {item_type}): void => {{ result.add({nested}); }});\nreturn result;",
                item_type = render_type_name(model, inner)
            )
        }
        ValueType::InputStream { item: inner, .. } => {
            let nested = lower_nested_call(
                model,
                helpers,
                inner,
                "item",
                &format!("{path}.item"),
                operation_id,
            );
            format!(
                "return session.createInputStream<{item_type}>({expression}, (item: {item_type}): ArkValue => {nested}) as ArkValue;",
                item_type = render_type_name(model, inner)
            )
        }
        ValueType::OutputStream { .. } => format!("return {expression} as ArkValue;"),
        ValueType::Named(key) => {
            let named = model.ty_for(key);
            let Some(named) = named else {
                return "throw new UniffiError(\"UniffiTypeDescriptor\", \"missing named type\");"
                    .to_owned();
            };
            match &named.kind {
                AstTypeKind::Custom { builtin, config } => {
                    let converted = custom_expression(&config.from_custom, expression);
                    let builtin_call =
                        lower_nested_call(model, helpers, builtin, &converted, path, operation_id);
                    format!("return {builtin_call};")
                }
                AstTypeKind::Object { kind } => {
                    let callback = operation_id.and_then(|id| model.callback_use(id, path));
                    if matches!(kind, ObjectKind::TraitForeignOnly) && callback.is_none() {
                        return "throw new UniffiError(\"UniffiCallbackContract\", \"foreign trait requires a callback contract\");".to_owned();
                    }
                    if let Some(callback) = callback {
                        let contract = callback_contract_literal(callback);
                        return format!(
                            "const __contract: ArkCallbackContract = {contract};\nreturn session.registerCallback({}, __makeCallbackAdapter{}({expression}, session), __contract) as ArkValue;",
                            callback.callback_type.index(),
                            callback.callback_type.index()
                        );
                    }
                    format!(
                        "return __objectHandle({expression}, session, {});",
                        named.id.index()
                    )
                }
                AstTypeKind::Callback => {
                    if let Some(operation_id) = operation_id {
                        if let Some(callback) = model.callback_use(operation_id, path) {
                            let contract = callback_contract_literal(callback);
                            return format!(
                                "const __contract: ArkCallbackContract = {contract};\nreturn session.registerCallback({}, __makeCallbackAdapter{}({expression}, session), __contract) as ArkValue;",
                                callback.callback_type.index(),
                                callback.callback_type.index()
                            );
                        }
                    }
                    "throw new UniffiError(\"UniffiCallbackContract\", \"missing canonical callback contract\");".to_owned()
                }
                AstTypeKind::Record { fields } => {
                    let mut body = format!("const result: ArkRecord = new ArkRecord();\n");
                    for field in fields {
                        let field_type = render_type_name(model, &field.ty);
                        let field_path = format!("{path}.field[{}]", field.name);
                        let field_call = lower_nested_call(
                            model,
                            helpers,
                            &field.ty,
                            &format!("fieldValue_{}", safe_ident(&field.name)),
                            &field_path,
                            operation_id,
                        );
                        body.push_str(&format!(
                            "const fieldValue_{field_name}: {field_type} = {expression}.{field_name} as {field_type};\n",
                            field_name = safe_ident(&field.name)
                        ));
                        if let Some(default) = &field.default {
                            if matches!(default, DefaultValue::Unspecified) {
                                body.push_str(&format!(
                                    "if ({field_name} !== undefined) result.set(\"{field}\", {field_call});\n",
                                    field_name = format!("fieldValue_{}", safe_ident(&field.name)),
                                    field = escape_string(&field.name),
                                    field_call = field_call
                                ));
                            } else {
                                let literal = render_default_for_type(default, &field.ty);
                                body.push_str(&format!(
                                    "if ({field_name} === undefined) result.set(\"{field}\", {lower_default}); else result.set(\"{field}\", {field_call});\n",
                                    field_name = format!("fieldValue_{}", safe_ident(&field.name)),
                                    field = escape_string(&field.name),
                                    lower_default = lower_nested_call(model, helpers, &field.ty, &literal, &field_path, operation_id)
                                ));
                            }
                        } else {
                            body.push_str(&format!(
                                "if ({field_name} === undefined) throw new UniffiError(\"UniffiRecordField\", \"missing record field {field}\"); result.set(\"{field}\", {field_call});\n",
                                field_name = format!("fieldValue_{}", safe_ident(&field.name)),
                                field = escape_string(&field.name)
                            ));
                        }
                    }
                    body.push_str("return result;");
                    body
                }
                AstTypeKind::Enum { variants } => {
                    let mut body = String::new();
                    for variant in variants {
                        body.push_str(&format!("if ({expression}.tag === \"{}\") {{ const result: ArkRecord = new ArkRecord(); result.set(\"tag\", \"{}\");\n", variant.name, variant.name));
                        for field in &variant.fields {
                            let field_path =
                                format!("{path}.variant[{}].field[{}]", variant.name, field.name);
                            let nested = lower_nested_call(
                                model,
                                helpers,
                                &field.ty,
                                &format!("{expression}.{}", field.name),
                                &field_path,
                                operation_id,
                            );
                            body.push_str(&format!(
                                "result.set(\"{}\", {});\n",
                                field.name, nested
                            ));
                        }
                        body.push_str("return result; }\n");
                    }
                    body.push_str("throw new UniffiError(\"UniffiEnumVariant\", \"unrecognized enum variant\");");
                    body
                }
                AstTypeKind::Error { variants } => {
                    let error_name = model.type_name(key);
                    let mut body = format!(
                        "if (!({expression} instanceof {error_name})) throw new UniffiError(\"UniffiErrorType\", \"expected {error_name}\");\nconst declared: {error_name} = {expression} as {error_name};\nconst variant: string | null = declared.variant;\nconst result: ArkRecord = new ArkRecord();\n"
                    );
                    for variant in variants {
                        body.push_str(&format!(
                            "if (variant === \"{}\") {{ result.set(\"tag\", \"{}\");\n",
                            variant.name, variant.name
                        ));
                        if variant.fields.is_empty() {
                            body.push_str("return result; }\n");
                            continue;
                        }
                        body.push_str(
                            "if (declared.data === null) throw new UniffiError(\"UniffiErrorPayload\", \"error payload is missing\"); const payloadValue: ArkValue = declared.data; if (!(payloadValue instanceof ArkRecord)) throw new UniffiError(\"UniffiErrorPayload\", \"error payload must be an ArkRecord\"); const payload: ArkRecord = payloadValue;\n",
                        );
                        for field in &variant.fields {
                            let field_path =
                                format!("{path}.variant[{}].field[{}]", variant.name, field.name);
                            let nested = lower_nested_call(
                                model,
                                helpers,
                                &field.ty,
                                &format!(
                                    "payload.get(\"{}\") as {}",
                                    field.name,
                                    render_type_name(model, &field.ty)
                                ),
                                &field_path,
                                operation_id,
                            );
                            body.push_str(&format!(
                                "result.set(\"{}\", {});\n",
                                field.name, nested
                            ));
                        }
                        body.push_str("return result; }\n");
                    }
                    body.push_str("throw new UniffiError(\"UniffiErrorVariant\", \"unrecognized error variant\");");
                    body
                }
            }
        }
    }
}

fn lower_nested_call(
    model: &Model<'_>,
    helpers: &mut Helpers,
    ty: &ValueType,
    expression: &str,
    path: &str,
    operation_id: Option<uniffi_js_abi::OperationId>,
) -> String {
    let label = helpers.next(&format!("nested_lower_{}", safe_ident(path)));
    let parameter_type = render_type_name(model, ty);
    let body = lower_body(model, helpers, ty, "value", path, operation_id);
    helpers.lower.push(format!(
        "function {label}(value: {parameter_type}, session: BackendSession): ArkValue {{\n{body}\n}}"
    ));
    format!("{label}({expression} as {parameter_type}, session)")
}

fn lift_helper(
    model: &Model<'_>,
    helpers: &mut Helpers,
    label: &str,
    ty: &ValueType,
    path: &str,
    operation_id: Option<uniffi_js_abi::OperationId>,
) -> String {
    let helper_name = helpers.next(&format!("lift_{}", safe_ident(label)));
    let return_type = render_type_name(model, ty);
    let body = lift_body(model, helpers, ty, "value", path, operation_id);
    helpers.lift.push(format!(
        "function {helper_name}(value: ArkValue, session: BackendSession): {return_type} {{\n{body}\n}}"
    ));
    helper_name
}

fn lift_body(
    model: &Model<'_>,
    helpers: &mut Helpers,
    ty: &ValueType,
    expression: &str,
    path: &str,
    operation_id: Option<uniffi_js_abi::OperationId>,
) -> String {
    match ty {
        ValueType::Scalar(scalar) => {
            format!("return __arkLift{}({expression});", scalar_name(*scalar))
        }
        ValueType::Timestamp => format!("return __arkLiftTimestamp({expression});"),
        ValueType::Duration => format!("return __arkLiftDuration({expression});"),
        ValueType::Optional(inner) => {
            let nested = lift_nested_call(model, helpers, inner, expression, path, operation_id);
            format!("if ({expression} === null) return null; if ({expression} === undefined) throw new UniffiError(\"UniffiUndefined\", \"backend omitted required value\"); return {nested};")
        }
        ValueType::Sequence(inner) => {
            let nested = lift_nested_call(
                model,
                helpers,
                inner,
                "item",
                &format!("{path}.item"),
                operation_id,
            );
            format!("const source: Array<ArkValue> = {expression} as Array<ArkValue>; const result: Array<{}> = new Array<{}>(); for (const item of source) result.push({nested}); return result;", render_type_name(model, inner), render_type_name(model, inner))
        }
        ValueType::Map(key, value) => {
            let key_call = lift_nested_call(
                model,
                helpers,
                key,
                "entryKey",
                &format!("{path}.key"),
                operation_id,
            );
            let value_call = lift_nested_call(
                model,
                helpers,
                value,
                "entryValue",
                &format!("{path}.value"),
                operation_id,
            );
            format!("const source: Map<ArkValue, ArkValue> = {expression} as Map<ArkValue, ArkValue>; const result: Map<{}, {}> = new Map<{}, {}>(); source.forEach((entryValue: ArkValue, entryKey: ArkValue): void => {{ result.set({key_call}, {value_call}); }}); return result;", render_type_name(model, key), render_type_name(model, value), render_type_name(model, key), render_type_name(model, value))
        }
        ValueType::Set(inner) => {
            let nested = lift_nested_call(
                model,
                helpers,
                inner,
                "item",
                &format!("{path}.set-item"),
                operation_id,
            );
            format!("const source: Set<ArkValue> = {expression} as Set<ArkValue>; const result: Set<{}> = new Set<{}>(); source.forEach((item: ArkValue): void => result.add({nested})); return result;", render_type_name(model, inner), render_type_name(model, inner))
        }
        ValueType::InputStream { item: inner, .. } => {
            let nested = lift_nested_call(
                model,
                helpers,
                inner,
                "item",
                &format!("{path}.item"),
                operation_id,
            );
            format!("return session.createInputView<{}>({expression}, (item: ArkValue): {} => {nested});", render_type_name(model, inner), render_type_name(model, inner))
        }
        ValueType::OutputStream {
            item: inner,
            error,
            is_send,
        } => format!(
            "return {expression} as {};",
            render_type_name(
                model,
                &ValueType::OutputStream {
                    item: inner.clone(),
                    error: error.clone(),
                    is_send: *is_send,
                },
            )
        ),
        ValueType::Named(key) => {
            let Some(named) = model.ty_for(key) else {
                return "throw new UniffiError(\"UniffiTypeDescriptor\", \"missing named type\");"
                    .to_owned();
            };
            match &named.kind {
                AstTypeKind::Custom { builtin, config } => {
                    let nested =
                        lift_nested_call(model, helpers, builtin, expression, path, operation_id);
                    format!(
                        "return ({}) as {};",
                        custom_expression(&config.into_custom, &nested),
                        render_type_name(model, ty)
                    )
                }
                AstTypeKind::Object { kind } => {
                    let callback = operation_id.and_then(|id| model.callback_use(id, path));
                    if matches!(kind, ObjectKind::TraitForeignOnly) && callback.is_none() {
                        return "throw new UniffiError(\"UniffiCallbackContract\", \"foreign trait requires a callback contract\");".to_owned();
                    }
                    if callback.is_some() {
                        format!("return {expression} as {};", render_type_name(model, ty))
                    } else {
                        format!(
                            "return session.liftObject<{}>({}, {expression});",
                            render_type_name(model, ty),
                            named.id.index()
                        )
                    }
                }
                AstTypeKind::Callback => {
                    format!("return {expression} as {};", render_type_name(model, ty))
                }
                AstTypeKind::Record { fields } => {
                    let mut body = format!(
                        "const raw: ArkRecord = {expression} as ArkRecord;\nconst result: {} = {{",
                        render_type_name(model, ty)
                    );
                    for field in fields {
                        let nested = lift_nested_call(
                            model,
                            helpers,
                            &field.ty,
                            &format!("raw.get(\"{}\")", field.name),
                            &format!("{path}.field[{}]", field.name),
                            operation_id,
                        );
                        body.push_str(&format!(" {}: {},", field.name, nested));
                    }
                    body.push_str(" }; return result;");
                    body
                }
                AstTypeKind::Enum { variants } => {
                    let mut body = format!("const raw: ArkRecord = {expression} as ArkRecord; const tag: string = raw.get(\"tag\") as string;\n");
                    for variant in variants {
                        let variant_name =
                            format!("{}_{}", model.type_name(key), safe_ident(&variant.name));
                        let mut fields = format!("{{ tag: \"{}\"", variant.name);
                        for field in &variant.fields {
                            let nested = lift_nested_call(
                                model,
                                helpers,
                                &field.ty,
                                &format!("raw.get(\"{}\")", field.name),
                                &format!("{path}.variant[{}].field[{}]", variant.name, field.name),
                                operation_id,
                            );
                            fields.push_str(&format!(", {}: {}", field.name, nested));
                        }
                        fields.push_str(" }");
                        body.push_str(&format!(
                            "if (tag === \"{}\") return {} as {};\n",
                            variant.name, fields, variant_name
                        ));
                    }
                    body.push_str("throw new UniffiError(\"UniffiEnumVariant\", \"unrecognized enum variant from backend\");");
                    body
                }
                AstTypeKind::Error { variants } => {
                    let error_name = model.type_name(key);
                    let mut body = format!(
                        "const raw: ArkRecord = {expression} as ArkRecord; const tag: string = raw.get(\"tag\") as string;\n"
                    );
                    for variant in variants {
                        if variant.fields.is_empty() {
                            body.push_str(&format!(
                                "if (tag === \"{}\") return new {}(\"\", tag, null);\n",
                                variant.name, error_name
                            ));
                            continue;
                        }
                        body.push_str(&format!(
                            "if (tag === \"{}\") {{ const data: ArkRecord = new ArkRecord();\n",
                            variant.name
                        ));
                        for field in &variant.fields {
                            let nested = lift_nested_call(
                                model,
                                helpers,
                                &field.ty,
                                &format!("raw.get(\"{}\")", field.name),
                                &format!("{path}.variant[{}].field[{}]", variant.name, field.name),
                                operation_id,
                            );
                            body.push_str(&format!("data.set(\"{}\", {});\n", field.name, nested));
                        }
                        body.push_str(&format!("return new {}(\"\", tag, data); }}\n", error_name));
                    }
                    body.push_str("throw new UniffiError(\"UniffiErrorVariant\", \"unrecognized error variant from backend\");");
                    body
                }
            }
        }
    }
}

fn lift_nested_call(
    model: &Model<'_>,
    helpers: &mut Helpers,
    ty: &ValueType,
    expression: &str,
    path: &str,
    operation_id: Option<uniffi_js_abi::OperationId>,
) -> String {
    let label = helpers.next(&format!("nested_lift_{}", helpers.counter));
    let parameter_type = render_type_name(model, ty);
    let body = lift_body(model, helpers, ty, "value", path, operation_id);
    helpers.lift.push(format!(
        "function {label}(value: ArkValue, session: BackendSession): {parameter_type} {{\n{body}\n}}"
    ));
    format!("{label}({expression} as ArkValue, session)")
}

fn custom_expression(expression: &str, value: &str) -> String {
    if expression.is_empty() {
        value.to_owned()
    } else {
        expression.replace("{}", value)
    }
}

fn callback_contract_literal(callback: &AstCallbackUseSite) -> String {
    format!(
        "{{ retention: \"{}\", threading: \"{}\", reentrancy: \"{}\" }}",
        match callback.contract.retention {
            CallbackRetention::Scoped => "scoped",
            CallbackRetention::Retained => "retained",
        },
        match callback.contract.threading {
            CallbackThreading::CallingThread => "callingThread",
            CallbackThreading::MayCrossThread => "mayCrossThread",
        },
        match callback.contract.reentrancy {
            CallbackReentrancy::Forbidden => "forbidden",
            CallbackReentrancy::Allowed => "allowed",
        }
    )
}

fn render_default_for_type(value: &DefaultValue, ty: &ValueType) -> String {
    if let DefaultValue::Some(inner) = value {
        let inner_type = match ty {
            ValueType::Optional(inner_type) => inner_type.as_ref(),
            _ => ty,
        };
        return render_default_for_type(inner, inner_type);
    }
    match value {
        DefaultValue::Unspecified => "undefined".to_owned(),
        DefaultValue::Boolean(value) => value.to_string(),
        DefaultValue::String(value) | DefaultValue::Enum(value) => {
            format!("\"{}\"", escape_string(value))
        }
        DefaultValue::Integer { value, .. } => {
            if matches!(ty, ValueType::Scalar(ScalarType::I64 | ScalarType::U64)) {
                format!("{value}n")
            } else {
                value.to_string()
            }
        }
        DefaultValue::Float(value) => value.clone(),
        DefaultValue::EmptySequence => "new Array<ArkValue>()".to_owned(),
        DefaultValue::EmptyMap => "new Map<ArkValue, ArkValue>()".to_owned(),
        DefaultValue::EmptySet => "new Set<ArkValue>()".to_owned(),
        DefaultValue::None => "null".to_owned(),
        DefaultValue::Some(_) => unreachable!(),
    }
}

fn escape_string(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn scalar_name(scalar: ScalarType) -> &'static str {
    match scalar {
        ScalarType::Bool => "Bool",
        ScalarType::I8 => "I8",
        ScalarType::U8 => "U8",
        ScalarType::I16 => "I16",
        ScalarType::U16 => "U16",
        ScalarType::I32 => "I32",
        ScalarType::U32 => "U32",
        ScalarType::I64 => "I64",
        ScalarType::U64 => "U64",
        ScalarType::F32 => "F32",
        ScalarType::F64 => "F64",
        ScalarType::String => "String",
        ScalarType::Bytes => "Bytes",
    }
}

fn render_factory_and_namespace(model: &Model<'_>) -> String {
    let mut out = String::new();
    for ty in &model.types {
        if !matches!(
            &ty.kind,
            AstTypeKind::Object { kind } if *kind != ObjectKind::TraitForeignOnly
        ) {
            continue;
        }
        let name = model.type_name(&ty.source_key);
        out.push_str(&format!(
            "const __factory{}: ArkObjectFactory = {{ ownerTypeId: {}, create: (session: BackendSession, handle: ArkValue): ObjectLease => {}.__arkCreate(new ArkObjectToken(session, {}), handle) }};\n",
            ty.id.index(),
            ty.id.index(),
            name,
            ty.id.index()
        ));
    }
    out.push_str("export function createNamespace(session: BackendSession): Namespace {\n  __installClosePolicy(session, __closePolicy);\n");
    for ty in &model.types {
        if matches!(
            &ty.kind,
            AstTypeKind::Object { kind } if *kind != ObjectKind::TraitForeignOnly
        ) {
            out.push_str(&format!(
                "  session.registerObjectFactory({}, __factory{});\n",
                ty.id.index(),
                ty.id.index()
            ));
        }
    }
    for component in &model.components {
        let component_name = model.component_name(component.id);
        let component_types = model
            .types
            .iter()
            .copied()
            .filter(|ty| ty.source_key.component().namespace() == component.namespace)
            .collect::<Vec<_>>();
        // Build each nested API value as a separately typed binding.  ArkTS
        // rejects nested object literals when their contextual interface type
        // is inferred through a component API object.
        for ty in &component_types {
            let value_ops = model
                .operations
                .iter()
                .copied()
                .filter(|operation| {
                    matches!(operation.source_key.owner(), OperationOwner::Value(key) if key == &ty.source_key)
                })
                .collect::<Vec<_>>();
            if !value_ops.is_empty() || matches!(ty.kind, AstTypeKind::Enum { .. }) {
                let value_name = model.type_name(&ty.source_key);
                let binding = format!(
                    "__{}_{}",
                    safe_ident(&component_name),
                    safe_ident(&value_name)
                );
                out.push_str(&format!("  const {binding}: {value_name}Value = {{\n"));
                if let AstTypeKind::Enum { variants } = &ty.kind {
                    let enum_value_name = format!("__arkEnum_{}", safe_ident(&value_name));
                    for variant in variants {
                        out.push_str(&format!(
                            "    {}: {}.{},\n",
                            variant.name, enum_value_name, variant.name
                        ));
                    }
                }
                for operation in value_ops {
                    let mut args = Vec::new();
                    if operation.kind != OperationKind::Constructor {
                        args.push(format!("self_: {}", value_name));
                    }
                    args.extend(operation.arguments.iter().map(|argument| {
                        format!(
                            "{}{}: {}",
                            argument.name,
                            if argument.default.is_some() { "?" } else { "" },
                            render_type_name(model, &argument.ty)
                        )
                    }));
                    let mut invoke_args = Vec::new();
                    if operation.kind != OperationKind::Constructor {
                        invoke_args.push("self_".to_owned());
                    }
                    invoke_args.push("session".to_owned());
                    invoke_args.extend(
                        operation
                            .arguments
                            .iter()
                            .map(|argument| argument.name.clone()),
                    );
                    out.push_str(&format!(
                        "    {}: ({}): {} => __invokeValue{}({}),\n",
                        operation.name,
                        args.join(", "),
                        operation_return_type(model, operation),
                        operation.id.index(),
                        invoke_args.join(", ")
                    ));
                }
                out.push_str("  };\n");
            }
            if matches!(ty.kind, AstTypeKind::Object { .. }) {
                let constructors = model
                    .operations
                    .iter()
                    .copied()
                    .filter(|operation| {
                        operation.kind == OperationKind::Constructor
                            && matches!(operation.source_key.owner(), OperationOwner::Object(key) if key == &ty.source_key)
                    })
                    .collect::<Vec<_>>();
                if !constructors.is_empty() {
                    let type_name = model.type_name(&ty.source_key);
                    let binding = format!(
                        "__{}_{}",
                        safe_ident(&component_name),
                        safe_ident(&type_name)
                    );
                    out.push_str(&format!("  const {binding}: {type_name}Constructor = {{\n"));
                    for operation in constructors {
                        let args = operation
                            .arguments
                            .iter()
                            .map(|argument| {
                                format!(
                                    "{}{}: {}",
                                    argument.name,
                                    if argument.default.is_some() { "?" } else { "" },
                                    render_type_name(model, &argument.ty)
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        let call_args = operation
                            .arguments
                            .iter()
                            .map(|argument| argument.name.clone())
                            .collect::<Vec<_>>()
                            .join(", ");
                        let suffix = if call_args.is_empty() {
                            String::new()
                        } else {
                            format!(", {call_args}")
                        };
                        out.push_str(&format!(
                            "    {}: ({}): {} => __invokeOperation{}(session{}),\n",
                            operation.name,
                            args,
                            operation_return_type(model, operation),
                            operation.id.index(),
                            suffix
                        ));
                    }
                    out.push_str("  };\n");
                }
            }
        }
        out.push_str(&format!(
            "  const __{component_name}: {component_name}Api = {{\n"
        ));
        let operations = model.operations_for_component(component);
        for operation in operations.iter().copied().filter(|operation| {
            operation.receiver_type.is_none()
                && matches!(operation.source_key.owner(), OperationOwner::Namespace)
                && !matches!(
                    operation.kind,
                    OperationKind::OutputStreamNext
                        | OperationKind::OutputStreamCancel
                        | OperationKind::InputStreamPull
                        | OperationKind::InputStreamCancel
                )
        }) {
            let args = operation
                .arguments
                .iter()
                .map(|argument| {
                    format!(
                        "{}{}: {}",
                        argument.name,
                        if argument.default.is_some() { "?" } else { "" },
                        render_type_name(model, &argument.ty)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let call_args = operation
                .arguments
                .iter()
                .map(|argument| argument.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            let suffix = if call_args.is_empty() {
                String::new()
            } else {
                format!(", {call_args}")
            };
            out.push_str(&format!(
                "    {}: ({}): {} => __invokeOperation{}(session{}),\n",
                operation.name,
                args,
                operation_return_type(model, operation),
                operation.id.index(),
                suffix
            ));
        }
        for ty in &component_types {
            let value_ops = model
                .operations
                .iter()
                .copied()
                .filter(|operation| {
                    matches!(operation.source_key.owner(), OperationOwner::Value(key) if key == &ty.source_key)
                })
                .collect::<Vec<_>>();
            if !value_ops.is_empty() || matches!(ty.kind, AstTypeKind::Enum { .. }) {
                let value_name = model.type_name(&ty.source_key);
                let binding = format!(
                    "__{}_{}",
                    safe_ident(&component_name),
                    safe_ident(&value_name)
                );
                out.push_str(&format!("    {value_name}: {binding},\n"));
            }
            if matches!(ty.kind, AstTypeKind::Object { .. }) {
                let constructors = model
                    .operations
                    .iter()
                    .copied()
                    .filter(|operation| {
                        operation.kind == OperationKind::Constructor
                            && matches!(operation.source_key.owner(), OperationOwner::Object(key) if key == &ty.source_key)
                    })
                    .collect::<Vec<_>>();
                if !constructors.is_empty() {
                    let type_name = model.type_name(&ty.source_key);
                    let binding = format!(
                        "__{}_{}",
                        safe_ident(&component_name),
                        safe_ident(&type_name)
                    );
                    out.push_str(&format!("    {type_name}: {binding},\n"));
                }
            }
        }
        out.push_str("  };\n");
    }
    out.push_str("  return {\n");
    for component in &model.components {
        let name = model.component_name(component.id);
        out.push_str(&format!("    {name}: __{name},\n"));
    }
    out.push_str("  };\n}\n");
    out
}
