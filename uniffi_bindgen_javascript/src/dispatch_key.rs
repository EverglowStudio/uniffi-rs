/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Shared low-level dispatch key helpers.
//!
//! `common/api.ts` dispatches by stable snake_case keys. Flavor adapters
//! then map those keys to the backend's actual export shape. Keep these
//! helpers pure and ABI-agnostic so common API generation, Electron async
//! dispatch, and N-API name mapping cannot drift.

use heck::ToSnakeCase;
use uniffi_bindgen::{
    interface::{Constructor, Method, Type},
    ComponentInterface,
};

pub fn snake_to_camel(snake: &str) -> String {
    crate::js_names::member_name(&snake.to_snake_case())
}

pub fn free_function_key(name: &str) -> String {
    name.to_string()
}

pub fn stream_next_key(name: &str) -> String {
    format!("{}_stream_next", free_function_key(name))
}

pub fn stream_cancel_key(name: &str) -> String {
    format!("{}_stream_cancel", free_function_key(name))
}

pub fn member_key(owner_name: &str, member_name: &str) -> String {
    format!(
        "{}_{}",
        owner_name.to_snake_case(),
        member_name.to_snake_case()
    )
}

pub fn constructor_key(owner_name: &str, constructor: &Constructor) -> String {
    member_key(owner_name, constructor.name())
}

pub fn method_key(owner_name: &str, method: &Method) -> String {
    member_key(owner_name, method.name())
}

pub fn object_method_key(method: &Method) -> String {
    member_key(method.object_name(), method.name())
}

/// Collect every `(low_level_key, napi_export_name)` pair the generated
/// node/electron backends need to look up. Sorted and deduplicated so the
/// emitted literal is stable across runs.
pub fn collect_name_map_pairs(ci: &ComponentInterface) -> Vec<(String, String)> {
    let mut pairs = Vec::new();

    for f in ci.function_definitions() {
        let key = free_function_key(f.name());
        pairs.push((key.clone(), snake_to_camel(&key)));
        if matches!(f.return_type(), Some(Type::Stream { .. })) {
            let next = stream_next_key(f.name());
            pairs.push((next.clone(), snake_to_camel(&next)));
            let cancel = stream_cancel_key(f.name());
            pairs.push((cancel.clone(), snake_to_camel(&cancel)));
        }
    }

    for record in ci.record_definitions() {
        for c in record.constructors() {
            let key = constructor_key(record.name(), c);
            pairs.push((key.clone(), snake_to_camel(&key)));
        }
        for m in record.methods() {
            let key = method_key(record.name(), m);
            pairs.push((key.clone(), snake_to_camel(&key)));
        }
    }

    for enum_ in ci.enum_definitions() {
        if ci.is_name_used_as_error(enum_.name()) {
            continue;
        }
        for c in enum_.constructors() {
            let key = constructor_key(enum_.name(), c);
            pairs.push((key.clone(), snake_to_camel(&key)));
        }
        for m in enum_.methods() {
            let key = method_key(enum_.name(), m);
            pairs.push((key.clone(), snake_to_camel(&key)));
        }
    }

    for obj in ci.object_definitions() {
        for c in obj.constructors() {
            let key = constructor_key(obj.name(), c);
            pairs.push((key.clone(), snake_to_camel(&key)));
        }
        for m in obj.methods() {
            let key = method_key(obj.name(), m);
            pairs.push((key.clone(), snake_to_camel(&key)));
        }
    }

    pairs.sort();
    pairs.dedup();
    pairs
}

/// Collect backend dispatch keys that must be sent through the async
/// Electron bridge path.
pub fn collect_async_keys(ci: &ComponentInterface) -> Vec<String> {
    let mut keys = Vec::new();

    for f in ci.function_definitions() {
        if f.is_async() {
            keys.push(free_function_key(f.name()));
        }
        if matches!(f.return_type(), Some(Type::Stream { .. })) {
            keys.push(stream_next_key(f.name()));
        }
    }
    for record in ci.record_definitions() {
        for c in record.constructors() {
            if c.is_async() {
                keys.push(constructor_key(record.name(), c));
            }
        }
        for m in record.methods() {
            if m.is_async() {
                keys.push(method_key(record.name(), m));
            }
        }
    }
    for enum_ in ci.enum_definitions() {
        if ci.is_name_used_as_error(enum_.name()) {
            continue;
        }
        for c in enum_.constructors() {
            if c.is_async() {
                keys.push(constructor_key(enum_.name(), c));
            }
        }
        for m in enum_.methods() {
            if m.is_async() {
                keys.push(method_key(enum_.name(), m));
            }
        }
    }
    for obj in ci.object_definitions() {
        for c in obj.constructors() {
            if c.is_async() {
                keys.push(constructor_key(obj.name(), c));
            }
        }
        for m in obj.methods() {
            if m.is_async() {
                keys.push(method_key(obj.name(), m));
            }
        }
    }

    keys.sort();
    keys.dedup();
    keys
}

#[cfg(test)]
mod tests {
    use super::{free_function_key, member_key, snake_to_camel};

    #[test]
    fn builds_representative_dispatch_keys() {
        assert_eq!(free_function_key("slow_add"), "slow_add");
        assert_eq!(member_key("Counter", "new"), "counter_new");
        assert_eq!(
            member_key("Counter", "with_initial"),
            "counter_with_initial"
        );
        assert_eq!(
            member_key("GreetOptions", "effective_repeat"),
            "greet_options_effective_repeat"
        );
        assert_eq!(member_key("Shape", "area_method"), "shape_area_method");
    }

    #[test]
    fn converts_representative_dispatch_keys_to_napi_exports() {
        assert_eq!(snake_to_camel("add"), "add");
        assert_eq!(snake_to_camel("greet_with"), "greetWith");
        assert_eq!(snake_to_camel("slow_add"), "slowAdd");
        assert_eq!(snake_to_camel("counter_new"), "counterNew");
        assert_eq!(snake_to_camel("counter_with_initial"), "counterWithInitial");
        assert_eq!(snake_to_camel("counter_get"), "counterGet");
        assert_eq!(snake_to_camel("greeter_greet"), "greeterGreet");
        assert_eq!(snake_to_camel("run_job"), "runJob");
    }
}
