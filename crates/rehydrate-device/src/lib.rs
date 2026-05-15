//! Device transport for reHydrate.
//!
//! The `Device` trait is the single seam between the rest of the app and the
//! reMarkable tablet. Phase 1 needs only enumeration and download; later
//! phases will extend the trait with upload, delete, and move.

pub mod error;
pub mod model;
pub mod trait_def;

// `fake` is only used by the sync crate's tests against a
// directory-backed `Device` impl, plus a small internal smoke test.
// Gating it behind `cfg(any(test, feature = "fake"))` keeps the
// production build's public surface focused on the SSH path while
// still letting downstream tests reach for `FakeDevice` by enabling
// the feature.
#[cfg(any(test, feature = "fake"))]
pub mod fake;

#[cfg(feature = "ssh")]
pub mod known_hosts;
#[cfg(feature = "ssh")]
pub mod ssh;

pub use error::{DeviceError, DeviceResult};
pub use model::{DeviceInfo, RemoteEntry, RemoteEntryKind, RemoteFile};
pub use trait_def::Device;

/// Validate that a device-supplied directory entry name is a single
/// path component.
///
/// Issue #33: SFTP NAME packets are not constrained by the protocol
/// to leaf components — clients are expected to validate. A
/// compromised, fuzzed, or man-in-the-middle'd SFTP server (e.g.
/// before host-key TOFU is established on a fresh USB-ethernet
/// connection) can return entries like `../../etc/shadow`. Without
/// validation, the SSH traversals concatenate that into both the
/// device-side `sftp.open()` path (escaping `xochitl_dir`) and the
/// local `RemoteFile.path` stored in the manifest. `Manifest::
/// validate_paths` on the host catches the bad path at manifest-
/// record time, but only after we may already have read or written
/// at the bad path on the device — defense-in-depth at the SFTP
/// boundary closes the window completely.
///
/// Reject empty, `.`, `..`, and anything containing a `/` or `\`.
pub fn is_safe_entry_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\\')
}

#[cfg(test)]
mod tests {
    use super::is_safe_entry_name;

    #[test]
    fn legitimate_names_are_accepted() {
        // The exact shapes the reMarkable's xochitl produces.
        assert!(is_safe_entry_name("abc.metadata"));
        assert!(is_safe_entry_name(
            "6132ec00-9f86-4ad8-aacf-b40b5c3a5fc9.content"
        ));
        assert!(is_safe_entry_name("page-1.rm"));
        assert!(is_safe_entry_name("thumbnails"));
        // Single-char names are syntactically fine — the traversal
        // logic upstream gates on `.metadata` / `<uuid>.` etc.
        assert!(is_safe_entry_name("a"));
    }

    /// Issue #33: the core attack shape. A device-side `../` smuggled
    /// into a NAME packet would let the host read off-tree paths
    /// (e.g. `xochitl_dir/../etc/shadow`) — the gate rejects any
    /// embedded path separator on both Unix and Windows conventions.
    #[test]
    fn parent_traversal_payloads_are_rejected() {
        assert!(!is_safe_entry_name(".."));
        assert!(!is_safe_entry_name("../etc/shadow"));
        assert!(!is_safe_entry_name("../../home/root/.ssh/authorized_keys"));
        assert!(!is_safe_entry_name("foo/../bar"));
        assert!(!is_safe_entry_name("subdir/file"));
    }

    /// Backslash is also a separator on Windows and on some SFTP
    /// servers that normalise via Windows shell conventions. Reject
    /// it regardless of host platform since `RemoteFile.path` is
    /// later normalised via `replace('\\', "/")` in ssh.rs — without
    /// this gate the backslash would survive into the manifest.
    #[test]
    fn backslash_payloads_are_rejected() {
        assert!(!is_safe_entry_name("..\\windows\\system32"));
        assert!(!is_safe_entry_name("foo\\bar"));
    }

    /// `.` would survive a starts_with(uuid) prefix check and
    /// indirectly read the directory itself (`{dir}/.`). Drop it.
    #[test]
    fn current_dir_and_empty_are_rejected() {
        assert!(!is_safe_entry_name("."));
        assert!(!is_safe_entry_name(""));
    }
}
