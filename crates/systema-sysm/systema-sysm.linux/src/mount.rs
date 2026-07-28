use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use sysa::proto::MountConfig;
use tokio::process::Command;
use tracing::{info, warn};

use crate::state::{MountInstance, MountRegistry, MountResult, MountState};

pub async fn do_mount(
    registry: MountRegistry,
    unit_name: &str,
    config: &MountConfig,
) -> Result<()> {
    let mount_point = config.r#where.clone();
    let mount_point_path = Path::new(&mount_point);

    // Create mount point directory if needed.
    let dir_mode = if config.directory_mode.is_empty() {
        "0755"
    } else {
        &config.directory_mode
    };
    if !mount_point_path.exists() {
        Command::new("mkdir").arg("-p").arg(&mount_point).status().await
            .context("mkdir -p for mount point failed")?;
        Command::new("chmod").arg(dir_mode).arg(&mount_point).status().await
            .context("chmod for mount point failed")?;
    }

    // Mark as mounting.
    {
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(unit_name) {
            inst.state = MountState::Mounting;
            inst.from_fragment = true;
        } else {
            let mut inst = MountInstance::new(
                unit_name.to_string(),
                mount_point.clone(),
                config.r#what.clone(),
            );
            inst.state = MountState::Mounting;
            inst.from_fragment = true;
            inst.fstype = config.r#type.clone();
            inst.options = config.options.clone();
            reg.insert(unit_name.to_string(), inst);
        }
    }

    // Build mount command.
    let mut cmd = Command::new("mount");
    if config.sloppy_options {
        cmd.arg("-s");
    }
    if !config.r#type.is_empty() && config.r#type != "auto" {
        cmd.arg("-t");
        cmd.arg(&config.r#type);
    }
    if !config.options.is_empty() {
        cmd.arg("-o");
        cmd.arg(&config.options);
    }
    cmd.arg(&config.r#what);
    cmd.arg(&mount_point);

    info!("Mounting: {:?}", cmd.as_std());

    let timeout = if config.timeout_sec > 0 {
        Duration::from_secs(config.timeout_sec as u64)
    } else {
        Duration::from_secs(30)
    };

    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .context("mount timed out")?
        .context("mount command failed to start")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let err = anyhow::anyhow!("mount failed: {}", stderr.trim());
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(unit_name) {
            inst.state = MountState::Failed;
            inst.result = MountResult::ExitCode;
        }
        return Err(err);
    }

    // Mount command succeeded. Check if mountinfo confirms it.
    let mounted = {
        let reg = registry.lock();
        reg.get(unit_name)
            .map(|inst| matches!(inst.state, MountState::MountingDone))
            .unwrap_or(false)
    };

    if mounted {
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(unit_name) {
            inst.state = MountState::Mounted;
            inst.result = MountResult::Success;
        }
        info!("Mount succeeded (confirmed by mountinfo): {}", mount_point);
    } else {
        // Mount exited successfully but mountinfo doesn't show it yet.
        // mountinfo poll will eventually pick it up and transition to MountingDone → Mounted.
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(unit_name) {
            if inst.state == MountState::Mounting {
                // Protocol error: mount returned 0 but mount point didn't appear.
                warn!("mount returned success but mount point {} not in mountinfo", mount_point);
                inst.state = MountState::Failed;
                inst.result = MountResult::Protocol;
            }
        }
        info!("Mount command exited successfully: {}", mount_point);
    }

    Ok(())
}

pub async fn do_umount(
    registry: MountRegistry,
    unit_name: &str,
    config: Option<&MountConfig>,
) -> Result<()> {
    let (mount_point, from_mountinfo) = {
        let reg = registry.lock();
        reg.get(unit_name)
            .map(|inst| (inst.mount_point.clone(), inst.from_mountinfo))
            .unwrap_or_else(|| {
                warn!("No mount point found for {}", unit_name);
                (String::new(), false)
            })
    };

    if mount_point.is_empty() {
        anyhow::bail!("No mount point recorded for unit {}", unit_name);
    }

    // Mark as unmounting.
    {
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(unit_name) {
            inst.state = MountState::Unmounting;
        }
    }

    let mut cmd = Command::new("umount");
    cmd.arg(&mount_point);

    let lazy = config.map(|c| c.lazy_unmount).unwrap_or(false);
    let force = config.map(|c| c.force_unmount).unwrap_or(false);

    if force {
        cmd.arg("-f");
    }
    if lazy {
        cmd.arg("-l");
    }

    info!("Unmounting: {:?}", cmd.as_std());

    let timeout = Duration::from_secs(
        config
            .map(|c| {
                if c.timeout_sec > 0 {
                    c.timeout_sec as u64
                } else {
                    30
                }
            })
            .unwrap_or(30),
    );

    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .context("umount timed out")?
        .context("umount command failed to start")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let err = anyhow::anyhow!("umount failed: {}", stderr.trim());
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(unit_name) {
            // If the unit was already gone from mountinfo, treat as success
            // even though the umount process returned non-zero.  This happens
            // when someone else unmounted it before us.
            if from_mountinfo {
                inst.state = MountState::Failed;
                inst.result = MountResult::ExitCode;
            } else {
                inst.state = MountState::Dead;
                inst.result = MountResult::Success;
            }
        }
        return Err(err);
    }

    // umount succeeded
    let still_mounted = {
        let reg = registry.lock();
        reg.get(unit_name)
            .map(|inst| inst.from_mountinfo)
            .unwrap_or(false)
    };

    let mut reg = registry.lock();
    if let Some(inst) = reg.get_mut(unit_name) {
        if still_mounted && inst.n_retry_umount < 32 {
            // Layered mount — retry
            inst.n_retry_umount += 1;
            inst.state = MountState::Unmounting;
            info!(
                "Layered mount still present, retry {}/32 for {}",
                inst.n_retry_umount, mount_point
            );
        } else {
            inst.state = MountState::Dead;
            inst.from_mountinfo = false;
            inst.result = MountResult::Success;
        }
    }
    drop(reg);

    info!("Unmount succeeded: {}", mount_point);
    Ok(())
}

pub async fn do_remount(
    registry: MountRegistry,
    unit_name: &str,
    config: &MountConfig,
) -> Result<()> {
    let mount_point = {
        let reg = registry.lock();
        reg.get(unit_name)
            .map(|inst| inst.mount_point.clone())
    };

    let mount_point = match mount_point {
        Some(p) => p,
        None => anyhow::bail!("Cannot remount {}: not currently mounted", unit_name),
    };

    let mut cmd = Command::new("mount");
    cmd.arg("-o");
    cmd.arg(format!("remount,{}", config.options));
    cmd.arg(&mount_point);

    info!("Remounting: {:?}", cmd.as_std());

    let timeout = Duration::from_secs(if config.timeout_sec > 0 {
        config.timeout_sec as u64
    } else {
        30
    });

    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .context("remount timed out")?
        .context("remount command failed to start")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("remount failed: {}", stderr.trim());
    }

    info!("Remount succeeded: {}", mount_point);
    Ok(())
}
