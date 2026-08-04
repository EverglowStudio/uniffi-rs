/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Crate-private staging and filesystem helpers shared by artifact builders.

#[cfg(not(test))]
mod engine;
#[cfg(test)]
pub(in crate::cli) mod engine;

// Platform and artifact builders only see this explicit crate-private facade.
#[cfg_attr(test, allow(unused_imports))]
pub(in crate::cli) use engine::{
    absolute_output_path, canonicalize_allow_missing, canonicalize_invocation_output,
    copy_dir_recursive, ensure_file_has_single_link, ensure_member_file_matches,
    normalize_hsp_destinations, path_entry_exists, publish_simple_output_set,
    read_verified_regular_file, read_verified_regular_file_bounded, require_regular_source_file,
    sha256_bytes, write_durable_file, HspOutputPaths, InvocationDist, InvocationOutputSpec,
    ManagedPackageStage, PreparedHspInvocation, PreparedHspPackage, StagedHspOutputs,
    TemporaryWorkspace, MAX_HSP_ARCHIVE_COMPRESSED_BYTES, MAX_HSP_ARCHIVE_ENTRIES,
    MAX_HSP_ARCHIVE_MEMBER_BYTES, MAX_HSP_ARCHIVE_PATH_BYTES, MAX_HSP_ARCHIVE_TOTAL_BYTES,
};

#[cfg(windows)]
#[cfg_attr(test, allow(unused_imports))]
pub(in crate::cli) use engine::windows_file_information;
