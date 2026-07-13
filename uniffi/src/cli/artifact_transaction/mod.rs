/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Stable crate-private artifact publication boundary.
//!
//! The transaction states, durable record schemas, recovery decisions, error
//! strings, path names, traversal limits, and supported threat boundary in
//! this module are frozen by the phase 3 characterization suite.  Callers may
//! construct plans and invoke the publication facade; platform builders must
//! not duplicate or extend the state machines kept behind that facade.

#[cfg(not(test))]
mod engine;
#[cfg(test)]
pub(in crate::cli) mod engine;

#[cfg(test)]
mod legacy_managed_harmony_test_support;

// Platform and artifact builders only see this explicit crate-private
// publication facade. The state records and recovery implementation remain
// private to the engine.
#[cfg_attr(test, allow(unused_imports))]
pub(in crate::cli) use engine::{
    absolute_output_path, canonicalize_allow_missing, canonicalize_invocation_output,
    capture_directory_for_cleanup, capture_existing_path_guards,
    collect_tree_inventory_ignoring_with_limits, complete_type_work_file_from_marker,
    copy_dir_recursive, create_unique_invocation_directory, directory_mutation_token_for_owner,
    ensure_file_has_single_link, ensure_member_file_identity, new_generation_id,
    normalize_hsp_destinations, output_lock_path, owned_entry_shape_valid, path_entry_exists,
    persistent_fs_identity, read_owned_tree_marker, read_verified_regular_file,
    read_verified_regular_file_bounded, read_verified_regular_file_bounded_with_budget,
    remove_captured_directory_for_cleanup, remove_current_regular_file_for_cleanup_with_budget,
    remove_owned_tree_for_cleanup, require_regular_source_file, sha256_bytes,
    validate_existing_path_guards, validate_inventory_path, validate_owned_tree,
    write_durable_file, write_owned_tree_marker, DirectCommitOutcome, DirectOwnerPlan,
    GenericPublicationPlan, HspCandidate, HspOutputPaths, HspPathGuards,
    IdentityBoundInvocationRoot, InvocationDist, InvocationOutputSpec, InvocationTypeCache,
    ManagedPackageOwner, ManagedPackageTransaction, ManagedTransactionLayout, OutputLock,
    OwnedTreeEntry, OwnedTreeMarker, OwnedTreeMarkerEntry, OwnedTreeSnapshot, PersistentFsIdentity,
    PreparedHspInvocation, PreparedHspPackage, PublicationHooks, SharedTraversalBudget,
    TraversalBudget, TypeCacheIdentity, TypeCacheInitialization, TypeCachePlan,
    MAX_HSP_ARCHIVE_COMPRESSED_BYTES, MAX_HSP_ARCHIVE_ENTRIES, MAX_HSP_ARCHIVE_MEMBER_BYTES,
    MAX_HSP_ARCHIVE_PATH_BYTES, MAX_HSP_ARCHIVE_TOTAL_BYTES, OWNED_TREE_SCHEMA_VERSION,
    TYPE_CACHE_OWNER_MARKER, TYPE_CACHE_WORK_MARKER,
};

#[cfg(windows)]
#[cfg_attr(test, allow(unused_imports))]
pub(in crate::cli) use engine::windows_file_information;

#[cfg(test)]
pub(in crate::cli) use legacy_managed_harmony_test_support::{
    ManagedHarmonyTransaction, MANAGED_HARMONY_OWNER_KIND, MANAGED_HARMONY_OWNER_MARKER,
};
