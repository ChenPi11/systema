//! API filesystem mount setup for SysAInit.
//!
//! The kernel and the initramfs mount `/proc`, `/sys` and `/dev`, but
//! nothing mounts the cgroup v2 (unified) hierarchy: systemd mounts it
//! itself as PID 1 in `mount_setup()` (`src/shared/mount-setup.c`,
//! `mount_table` entry for cgroup2), so SysAInit does the same.
//!
//! The mount is attempted only when SysAInit has the privileges to mount
//! (effective root: real root, or root inside a container / user
//! namespace).  In a rootless environment the mount is skipped and
//! resource control degrades to the no-op controller, as if cgroup2 had
//! never been mounted.

use std::fs;

use anyhow::{anyhow, Context};
use nix::errno::Errno;
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use tracing::{debug, info};

/// Path of the unified cgroup v2 hierarchy.
const CGROUP_PATH: &str = "/sys/fs/cgroup";

/// Mount options for cgroup2, kept in sync with systemd's mount-table
/// entry (`nsdelegate,memory_recursiveprot`).
const CGROUP_OPTIONS: &str = "nsdelegate,memory_recursiveprot";

/// Whether SysAInit may perform mounts: the effective user must be root.
///
/// Real root and root-inside-a-userns/container both yield euid 0 and
/// both may mount cgroup2 in their own (cgroup) namespace; a rootless
/// environment runs with a non-root euid and must skip the mount.
fn has_mount_privileges() -> bool {
    // SAFETY: geteuid(2) is always successful and side-effect free.
    (unsafe { nix::libc::geteuid() }) == 0
}

/// Check whether `path` is currently a mount point.
///
/// Parses `/proc/self/mountinfo` instead of statfs, mirroring systemd's
/// `path_is_mount_point_full()`: bind mounts and stacked mounts are
/// handled correctly, and no filesystem interaction is required.
fn is_mount_point(path: &str) -> bool {
    let Ok(mountinfo) = fs::read_to_string("/proc/self/mountinfo") else {
        debug!("Cannot read /proc/self/mountinfo; assuming {path} is not a mount point");
        return false;
    };
    mountinfo.lines().any(|line| line.split(' ').nth(4) == Some(path))
}

/// Mount the cgroup v2 hierarchy at `/sys/fs/cgroup`, mirroring systemd.
///
/// Returns `Ok(())` when cgroup2 is already mounted, was mounted now, or
/// when SysAInit lacks the privileges to mount (rootless environment).
/// Returns `Err` when mounting should have been possible but failed.
pub fn mount_cgroup2() -> anyhow::Result<()> {
    if !has_mount_privileges() {
        debug!("Running without mount privileges (rootless); not mounting cgroup2");
        return Ok(());
    }

    if is_mount_point(CGROUP_PATH) {
        info!("cgroup2 already mounted at {CGROUP_PATH}; not mounting again");
        return Ok(());
    }

    fs::create_dir_all(CGROUP_PATH)
        .with_context(|| format!("Cannot create cgroup mount point {CGROUP_PATH}"))?;

    mount(
        Some("cgroup2"),
        CGROUP_PATH,
        Some("cgroup2"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
        Some(CGROUP_OPTIONS),
    )
    .map_err(|e| {
        if e == Errno::EBUSY {
            anyhow!("{CGROUP_PATH} is already occupied by another filesystem (cgroup v1?); hybrid cgroup hierarchy is not supported")
        } else {
            anyhow!("Cannot mount cgroup2 at {CGROUP_PATH}: {e}")
        }
    })?;

    // systemd's MNT_CHECK_WRITABLE: undo the mount when the filesystem
    // is not actually writable.
    // SAFETY: access(2) only touches errno and returns -1 on failure.
    if unsafe { nix::libc::access(CGROUP_PATH.as_ptr().cast(), nix::libc::W_OK) } != 0 {
        let err = std::io::Error::last_os_error();
        let _ = umount2(CGROUP_PATH, MntFlags::UMOUNT_NOFOLLOW);
        let _ = fs::remove_dir(CGROUP_PATH);
        return Err(anyhow!(
            "cgroup2 mount at {CGROUP_PATH} is not writable, undoing: {err}"
        ));
    }

    info!("Mounted cgroup2 at {CGROUP_PATH} ({CGROUP_OPTIONS})");
    Ok(())
}