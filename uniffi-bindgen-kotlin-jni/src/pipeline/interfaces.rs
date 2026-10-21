/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

pub fn map_class(int: general::Interface, context: &Context) -> Result<Class> {
    let lifecycle_close = context
        .config()?
        .lifecycle_closes
        .get(&int.orig_name)
        .and_then(|name| {
            int.methods
                .iter()
                .find(|method| method.callable.orig_name == *name)
        })
        .map(|method| method.callable.name.clone());
    let mut base_classes = vec![interface_name(&int), "uniffi.Disposable".to_string()];
    if lifecycle_close.is_none()
        && !int.imp.is_local()
        && !int.methods.iter().any(|m| m.callable.name == "close")
    {
        base_classes.push("AutoCloseable".into());
    }
    let callback_interface = int
        .imp
        .has_callback_interface()
        .then(|| callbacks::map_trait_interface(int.clone(), context))
        .transpose()?;

    let methods = int.methods.map_node(context)?;
    let self_type = int.self_type.map_node(context)?;
    if self_type.is_used_as_error {
        base_classes.push("kotlin.Exception".into());
    }
    if int.uniffi_trait_methods.ord_cmp.is_some() {
        base_classes.push(format!("Comparable<{}>", self_type.type_kt));
    }

    Ok(Class {
        lifecycle_close,
        self_type,
        base_classes,
        package_name: context.current_package_name()?.to_string(),
        constructors: int.constructors.map_node(context)?,
        methods,
        name: int.name,
        module_path: context.normalize_rust_module_path(&int.module_path)?,
        orig_name: int.orig_name,
        uniffi_trait_methods: int.uniffi_trait_methods.map_node(context)?,
        docstring: int.docstring.map_node(context)?,
        crate_name: context.current_crate_name()?.to_string(),
        imp: int.imp,
        callback_interface,
    })
}

pub fn map_interface(int: &general::Interface, context: &Context) -> Result<Interface> {
    Ok(Interface {
        name: interface_name(int),
        methods: int.methods.clone().map_node(context)?,
        docstring: int.docstring.clone(),
    })
}

fn interface_name(int: &general::Interface) -> String {
    if int.imp.has_callback_interface() {
        int.name.to_upper_camel_case()
    } else {
        format!("{}Interface", int.name.to_upper_camel_case())
    }
}

impl Interface {
    pub fn name_kt(&self) -> String {
        format!("`{}`", self.name.to_upper_camel_case())
    }
}

impl Class {
    pub fn is_lifecycle_close(&self, method: &Method) -> bool {
        self.lifecycle_close.as_deref() == Some(method.callable.name.as_str())
    }

    pub fn name_kt(&self) -> String {
        if self.imp.has_callback_interface() {
            format!("{}Impl", self.name.to_upper_camel_case())
        } else {
            names::class_name_kt(&self.name, self.self_type.is_used_as_error)
        }
    }

    pub fn name_rs(&self) -> String {
        names::escape_rust(&self.orig_name)
    }

    pub fn has_close_method(&self) -> bool {
        self.methods.iter().any(|m| m.callable.name == "close")
    }

    pub fn inner_type_name(&self) -> String {
        let name = format!("{}::{}", self.module_path, self.name_rs());
        if self.imp.is_local() {
            format!("uniffi::async_runtime::LocalObject<{name}>")
        } else {
            name
        }
    }

    pub fn jni_free_name(&self) -> String {
        format!("objectFree{}", self.self_type.id)
    }

    pub fn jni_clone_name(&self) -> String {
        format!("objectCloneHandle{}", self.self_type.id)
    }

    pub fn handle_map_kt(&self) -> String {
        format!("callbackInterfaceHandleMap{}", self.self_type.id)
    }

    pub fn impl_struct_rs(&self) -> String {
        format!("UniffiCallbackImpl{}", self.self_type.id)
    }

    pub fn primary_constructor(&self) -> Option<&Constructor> {
        self.constructors.iter().find(|c| {
            matches!(
                c.callable.kind,
                CallableKind::Constructor { primary: true, .. }
            )
        })
    }

    pub fn secondary_constructors(&self) -> impl Iterator<Item = &Constructor> {
        self.constructors.iter().filter(|c| {
            matches!(
                c.callable.kind,
                CallableKind::Constructor { primary: false, .. }
            )
        })
    }
}
