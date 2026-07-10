/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

typedef void (*cleanup_hook)(void *);
typedef int (*cleanup_wrapper)(void *, cleanup_hook, void *);

static void *last_add_arg;
static void *last_remove_arg;
static unsigned main_wrapper_calls;

// The generated wrapper's `__real_*` reference resolves to these executable
// exports. This makes the probe independent of a live napi_env while still
// executing the exact wrapper code linked into each generated DSO.
__attribute__((visibility("default"))) int
napi_add_env_cleanup_hook(void *env, cleanup_hook hook, void *arg) {
  (void)env;
  (void)hook;
  last_add_arg = arg;
  return 0;
}

__attribute__((visibility("default"))) int
napi_remove_env_cleanup_hook(void *env, cleanup_hook hook, void *arg) {
  (void)env;
  (void)hook;
  last_remove_arg = arg;
  return 0;
}

// Deliberately collide with both generated wrapper names. STV_PROTECTED on
// each generated definition must keep its own calls and dlsym(handle, ...)
// lookup local instead of selecting these executable symbols.
__attribute__((visibility("default"))) int
__wrap_napi_add_env_cleanup_hook(void *env, cleanup_hook hook, void *arg) {
  (void)env;
  (void)hook;
  main_wrapper_calls++;
  last_add_arg = arg;
  return 0;
}

__attribute__((visibility("default"))) int
__wrap_napi_remove_env_cleanup_hook(void *env, cleanup_hook hook, void *arg) {
  (void)env;
  (void)hook;
  main_wrapper_calls++;
  last_remove_arg = arg;
  return 0;
}

static void callback_one(void *arg) { (void)arg; }
static void callback_two(void *arg) { (void)arg; }

static void fail(const char *message) {
  fprintf(stderr, "cleanup-probe: %s\n", message);
  exit(1);
}

static cleanup_wrapper symbol(void *handle, const char *name) {
  dlerror();
  cleanup_wrapper result = (cleanup_wrapper)dlsym(handle, name);
  const char *error = dlerror();
  if (error != NULL || result == NULL) {
    fprintf(stderr, "cleanup-probe: dlsym %s failed: %s\n", name,
            error == NULL ? "missing symbol" : error);
    exit(1);
  }
  return result;
}

int main(int argc, char **argv) {
  if (argc != 3) {
    fail("usage: probe <generated-a.so> <generated-b.so>");
  }
  void *first = dlopen(argv[1], RTLD_LAZY | RTLD_LOCAL);
  if (first == NULL) {
    fail(dlerror());
  }
  void *second = dlopen(argv[2], RTLD_LAZY | RTLD_LOCAL);
  if (second == NULL) {
    fail(dlerror());
  }

  cleanup_wrapper add_first = symbol(first, "__wrap_napi_add_env_cleanup_hook");
  cleanup_wrapper remove_first =
      symbol(first, "__wrap_napi_remove_env_cleanup_hook");
  cleanup_wrapper add_second =
      symbol(second, "__wrap_napi_add_env_cleanup_hook");

  last_add_arg = NULL;
  if (add_first(NULL, callback_one, NULL) != 0 || last_add_arg == NULL) {
    fail("first callback did not receive a generated key");
  }
  void *first_one = last_add_arg;
  last_add_arg = NULL;
  if (add_first(NULL, callback_one, NULL) != 0 || last_add_arg != first_one) {
    fail("same callback key is not stable");
  }
  last_remove_arg = NULL;
  if (remove_first(NULL, callback_one, NULL) != 0 ||
      last_remove_arg != first_one) {
    fail("add/remove key mapping is not symmetric");
  }

  last_add_arg = NULL;
  if (add_first(NULL, callback_two, NULL) != 0 || last_add_arg == NULL) {
    fail("second callback did not receive a generated key");
  }
  void *first_two = last_add_arg;
  if (first_two == first_one) {
    fail("different null-arg callbacks shared one key");
  }

  last_add_arg = NULL;
  if (add_second(NULL, callback_one, NULL) != 0 || last_add_arg == NULL) {
    fail("second DSO callback did not receive a generated key");
  }
  void *second_one = last_add_arg;
  if (second_one == first_one) {
    fail("two generated DSOs shared one key");
  }

  int nonnull_key = 7;
  last_add_arg = NULL;
  if (add_first(NULL, callback_one, &nonnull_key) != 0 ||
      last_add_arg != &nonnull_key) {
    fail("nonnull cleanup key was changed");
  }
  if (main_wrapper_calls != 0) {
    fail("main executable interposed a generated wrapper");
  }

  printf("cleanup-probe: pass first=%p second-callback=%p second-dso=%p\n",
         first_one, first_two, second_one);
  dlclose(second);
  dlclose(first);
  return 0;
}
