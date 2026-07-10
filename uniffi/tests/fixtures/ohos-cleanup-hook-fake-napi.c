/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include <stdarg.h>

// Shell-side cleanup wrapper probes cannot enter the platform N-API linker
// namespace. This minimal test DSO satisfies the unrelated generated bridge
// imports; the probe executable itself exports the real add/remove stubs that
// record wrapper arguments.
#define NAPI_STUB(name)                                                        \
  __attribute__((visibility("default"))) int name(void *first, ...) {          \
    (void)first;                                                               \
    return 0;                                                                  \
  }

NAPI_STUB(napi_call_function)
NAPI_STUB(napi_call_threadsafe_function)
NAPI_STUB(napi_close_handle_scope)
NAPI_STUB(napi_coerce_to_string)
NAPI_STUB(napi_create_array_with_length)
NAPI_STUB(napi_create_bigint_words)
NAPI_STUB(napi_create_error)
NAPI_STUB(napi_create_function)
NAPI_STUB(napi_create_object)
NAPI_STUB(napi_create_promise)
NAPI_STUB(napi_create_reference)
NAPI_STUB(napi_create_string_latin1)
NAPI_STUB(napi_create_string_utf8)
NAPI_STUB(napi_create_threadsafe_function)
NAPI_STUB(napi_create_uint32)
NAPI_STUB(napi_define_class)
NAPI_STUB(napi_define_properties)
NAPI_STUB(napi_delete_reference)
NAPI_STUB(napi_fatal_exception)
NAPI_STUB(napi_get_and_clear_last_exception)
NAPI_STUB(napi_get_boolean)
NAPI_STUB(napi_get_cb_info)
NAPI_STUB(napi_get_global)
NAPI_STUB(napi_get_named_property)
NAPI_STUB(napi_get_null)
NAPI_STUB(napi_get_property)
NAPI_STUB(napi_get_prototype)
NAPI_STUB(napi_get_reference_value)
NAPI_STUB(napi_get_undefined)
NAPI_STUB(napi_get_value_bigint_words)
NAPI_STUB(napi_get_value_bool)
NAPI_STUB(napi_get_value_string_utf8)
NAPI_STUB(napi_get_value_uint32)
NAPI_STUB(napi_is_error)
NAPI_STUB(napi_is_exception_pending)
NAPI_STUB(napi_module_register)
NAPI_STUB(napi_new_instance)
NAPI_STUB(napi_open_handle_scope)
NAPI_STUB(napi_reference_unref)
NAPI_STUB(napi_reject_deferred)
NAPI_STUB(napi_release_threadsafe_function)
NAPI_STUB(napi_resolve_deferred)
NAPI_STUB(napi_set_element)
NAPI_STUB(napi_set_named_property)
NAPI_STUB(napi_set_property)
NAPI_STUB(napi_strict_equals)
NAPI_STUB(napi_throw)
NAPI_STUB(napi_throw_error)
NAPI_STUB(napi_typeof)
NAPI_STUB(napi_unref_threadsafe_function)
NAPI_STUB(napi_unwrap)
NAPI_STUB(napi_wrap)
