use std::ffi::CString;
use std::os::unix::io::RawFd;
use std::time::Duration;

use anyhow::Result;
use sysa::proto::AutomountConfig;
use tokio::io::unix::AsyncFd;
use tracing::{debug, info, warn};

use crate::state::{
    AutomountInstance, AutomountRegistry, AutomountState,
};

// ---------------------------------------------------------------------------
// Linux ioctl / autofs constants
// ---------------------------------------------------------------------------

// Values from <asm-generic/ioctl.h> and <linux/auto_dev-ioctl.h>
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const IOC_DIR_SHIFT: u32 = 30;
const IOC_TYPE_SHIFT: u32 = 8;
const IOC_NR_SHIFT: u32 = 0;
const IOC_SIZE_SHIFT: u32 = 16;

const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> libc::c_ulong {
    ((dir << IOC_DIR_SHIFT)
        | ((ty as u32) << IOC_TYPE_SHIFT)
        | ((nr as u32) << IOC_NR_SHIFT)
        | ((size as u32) << IOC_SIZE_SHIFT)) as libc::c_ulong
}

const fn iowr(ty: u8, nr: u8, size: usize) -> libc::c_ulong {
    ioc(IOC_READ | IOC_WRITE, ty, nr, size)
}

#[repr(C)]
#[derive(Debug, Default)]
struct AutofsDevIoctl {
    ver_major: u32,
    ver_minor: u32,
    size: u32,
    ioctlfd: i32,
    arg1: u64,
    arg2: u64,
}

const AUTOFS_TYPE: u8 = 0xf9;
const AUTOFS_DEV_IOCTL_SIZEOF: usize = std::mem::size_of::<AutofsDevIoctl>();
const AUTOFS_DEV_IOCTL_VERSION: libc::c_ulong = iowr(AUTOFS_TYPE, 0x00, AUTOFS_DEV_IOCTL_SIZEOF);
const AUTOFS_DEV_IOCTL_OPENMOUNT: libc::c_ulong = iowr(AUTOFS_TYPE, 0x04, AUTOFS_DEV_IOCTL_SIZEOF);
const AUTOFS_DEV_IOCTL_PROTOVER: libc::c_ulong = iowr(AUTOFS_TYPE, 0x01, AUTOFS_DEV_IOCTL_SIZEOF);
const AUTOFS_DEV_IOCTL_PROTOSUBVER: libc::c_ulong = iowr(AUTOFS_TYPE, 0x02, AUTOFS_DEV_IOCTL_SIZEOF);
const AUTOFS_DEV_IOCTL_TIMEOUT: libc::c_ulong = iowr(AUTOFS_TYPE, 0x0b, AUTOFS_DEV_IOCTL_SIZEOF);
const AUTOFS_DEV_IOCTL_EXPIRE: libc::c_ulong = iowr(AUTOFS_TYPE, 0x0c, AUTOFS_DEV_IOCTL_SIZEOF);

const AUTOFS_DEV_IOCTL_OPENMOUNT_SIZEOF: usize =
    AUTOFS_DEV_IOCTL_SIZEOF + 256; // room for path

// ---------------------------------------------------------------------------
// Autofs v5 packet
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Clone)]
struct AutofsPacket {
    // autofs_v5_packet_union header + v5_packet body
    // From linux/auto_fs.h: struct autofs_v5_packet has: wait_queue_token, len, name, dev_t, pid, uid, gid, ...
    proto: u32,
    type_: u32,
    wait_queue_token: u32,
    len: u32,
    name: [u8; 256],
    dev_t: u64,
    pid: u32,
    uid: u32,
    gid: u32,
}

const AUTOFS_PTYPE_MISSING_DIRECT: u32 = 5;
const AUTOFS_PTYPE_EXPIRE_DIRECT: u32 = 2;

// ---------------------------------------------------------------------------
// Dev autofs fd (global, shared across all automount instances)
// ---------------------------------------------------------------------------

static mut DEV_AUTOFS_FD: i32 = -1;

fn ensure_dev_autofs() -> Result<i32> {
    unsafe {
        if DEV_AUTOFS_FD >= 0 {
            return Ok(DEV_AUTOFS_FD);
        }

        let fd = libc::open(
            CString::new("/dev/autofs").unwrap().as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        );
        if fd < 0 {
            anyhow::bail!("Failed to open /dev/autofs (is autofs kernel module loaded?)");
        }

        // Verify version
        let mut params: AutofsDevIoctl = Default::default();
        let rc = libc::ioctl(
            fd,
            AUTOFS_DEV_IOCTL_VERSION,
            &mut params as *mut _ as *mut libc::c_void,
        );
        if rc < 0 {
            libc::close(fd);
            anyhow::bail!("AUTOFS_DEV_IOCTL_VERSION failed");
        }

        info!(
            "Autofs kernel version {}.{}",
            params.ver_major, params.ver_minor
        );

        DEV_AUTOFS_FD = fd;
        Ok(fd)
    }
}

// ---------------------------------------------------------------------------
// Core automount operations
// ---------------------------------------------------------------------------

fn open_ioctl_fd(dev_autofs_fd: i32, where_: &str, dev_id: u64) -> Result<i32> {
    let _path_c = CString::new(where_).unwrap();
    let path_bytes = where_.as_bytes();

    // Allocate buffer: struct + path + null
    let buf_size = AUTOFS_DEV_IOCTL_OPENMOUNT_SIZEOF;
    let mut buf = vec![0u8; buf_size];

    unsafe {
        let params = &mut *(buf.as_mut_ptr() as *mut AutofsDevIoctl);
        params.size = (AUTOFS_DEV_IOCTL_SIZEOF + path_bytes.len() + 1) as u32;
        params.ioctlfd = -1;
        params.arg1 = dev_id; // openmount.devid = dev_id

        // Copy path after the struct
        let path_ptr = buf.as_mut_ptr().add(AUTOFS_DEV_IOCTL_SIZEOF);
        std::ptr::copy_nonoverlapping(path_bytes.as_ptr(), path_ptr, path_bytes.len());
        *path_ptr.add(path_bytes.len()) = 0;

        let rc = libc::ioctl(
            dev_autofs_fd,
            AUTOFS_DEV_IOCTL_OPENMOUNT,
            buf.as_ptr() as *const libc::c_void,
        );
        if rc < 0 {
            anyhow::bail!("AUTOFS_DEV_IOCTL_OPENMOUNT failed for {}", where_);
        }

        let ioctl_fd = params.ioctlfd;
        if ioctl_fd < 0 {
            anyhow::bail!("AUTOFS_DEV_IOCTL_OPENMOUNT returned invalid fd");
        }

        Ok(ioctl_fd)
    }
}

fn check_autofs_protocol(dev_autofs_fd: i32, ioctl_fd: i32) -> Result<()> {
    unsafe {
        let mut params: AutofsDevIoctl = Default::default();
        params.ioctlfd = ioctl_fd;

        let rc = libc::ioctl(
            dev_autofs_fd,
            AUTOFS_DEV_IOCTL_PROTOVER,
            &mut params as *mut _ as *mut libc::c_void,
        );
        if rc < 0 {
            anyhow::bail!("AUTOFS_DEV_IOCTL_PROTOVER failed");
        }
        let major = params.arg1 as u32;

        let mut params2: AutofsDevIoctl = Default::default();
        params2.ioctlfd = ioctl_fd;
        let rc = libc::ioctl(
            dev_autofs_fd,
            AUTOFS_DEV_IOCTL_PROTOSUBVER,
            &mut params2 as *mut _ as *mut libc::c_void,
        );
        if rc < 0 {
            anyhow::bail!("AUTOFS_DEV_IOCTL_PROTOSUBVER failed");
        }
        let minor = params2.arg1 as u32;

        debug!("Autofs protocol version {}.{}", major, minor);
    }
    Ok(())
}

fn set_autofs_timeout(
    dev_autofs_fd: i32,
    ioctl_fd: i32,
    timeout_sec: u32,
) -> Result<()> {
    unsafe {
        let mut params: AutofsDevIoctl = Default::default();
        params.ioctlfd = ioctl_fd;
        params.arg1 = timeout_sec as u64;

        let rc = libc::ioctl(
            dev_autofs_fd,
            AUTOFS_DEV_IOCTL_TIMEOUT,
            &mut params as *mut _ as *mut libc::c_void,
        );
        if rc < 0 {
            anyhow::bail!("AUTOFS_DEV_IOCTL_TIMEOUT failed");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Mount(2) helper for autofs
// ---------------------------------------------------------------------------

fn mount_autofs(
    pipe_write_fd: RawFd,
    where_: &str,
    extra_options: &str,
    pgrp: i32,
) -> Result<()> {
    let source = format!("systemd-{}", unsafe { libc::getpid() });
    let fstype = CString::new("autofs").unwrap();
    let target = CString::new(where_).unwrap();
    let options = if extra_options.is_empty() {
        format!("fd={},pgrp={},minproto=5,maxproto=5,direct", pipe_write_fd, pgrp)
    } else {
        format!(
            "fd={},pgrp={},minproto=5,maxproto=5,direct,{}",
            pipe_write_fd, pgrp, extra_options
        )
    };
    let options_c = CString::new(options).unwrap();
    let source_c = CString::new(source).unwrap();

    unsafe {
        let rc = libc::mount(
            source_c.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            options_c.as_ptr() as *const libc::c_void,
        );
        if rc < 0 {
            anyhow::bail!(
                "mount(2) autofs failed for {}: {}",
                where_,
                std::io::Error::last_os_error()
            );
        }
    }
    Ok(())
}

fn unmount_autofs(where_: &str) -> Result<()> {
    let target = CString::new(where_).unwrap();
    unsafe {
        let rc = libc::umount2(target.as_ptr(), libc::MNT_DETACH | libc::UMOUNT_NOFOLLOW);
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            warn!("umount2(MNT_DETACH) failed for {}: {}", where_, err);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub async fn automount_enter_waiting(
    registry: AutomountRegistry,
    unit_name: &str,
    config: &AutomountConfig,
    mount_event_tx: tokio::sync::mpsc::UnboundedSender<AutomountTrigger>,
) -> Result<()> {
    let where_ = config.r#where.clone();

    // Create pipe for autofs communication.
    let mut pipe_fds = [-1i32; 2];
    unsafe {
        let rc = libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC);
        if rc < 0 {
            anyhow::bail!("pipe2 for automount failed");
        }
        // Make read side non-blocking.
        let flags = libc::fcntl(pipe_fds[0], libc::F_GETFL, 0);
        libc::fcntl(pipe_fds[0], libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    let dev_autofs_fd = ensure_dev_autofs()?;

    // Create mount point directory.
    let dir_mode = if config.directory_mode.is_empty() {
        "0755"
    } else {
        &config.directory_mode
    };
    let _ = std::process::Command::new("mkdir")
        .arg("-p")
        .arg(&where_)
        .status();
    let _ = std::process::Command::new("chmod")
        .arg(dir_mode)
        .arg(&where_)
        .status();

    // Mount autofs.
    mount_autofs(pipe_fds[1], &where_, &config.extra_options, std::process::id() as i32)?;

    // Close write end in parent.
    unsafe {
        libc::close(pipe_fds[1]);
    }

    // Stat the mount point to get dev_id.
    let dev_id = {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let path_c = CString::new(where_.as_str()).unwrap();
        unsafe {
            let rc = libc::stat(path_c.as_ptr(), &mut st);
            if rc < 0 {
                unmount_autofs(&where_)?;
                anyhow::bail!("stat of automount point {} failed", where_);
            }
        }
        st.st_dev
    };

    // Open ioctl fd.
    let ioctl_fd = open_ioctl_fd(dev_autofs_fd, &where_, dev_id)?;

    // Check protocol.
    check_autofs_protocol(dev_autofs_fd, ioctl_fd)?;

    // Set timeout.
    let timeout_idle_sec = config.timeout_idle_sec;
    if timeout_idle_sec > 0 {
        set_autofs_timeout(dev_autofs_fd, ioctl_fd, timeout_idle_sec)?;
    }

    // Register the instance.
    {
        let mut reg = registry.lock();
        let mut inst = AutomountInstance::new(
            unit_name.to_string(),
            where_.clone(),
        );
        inst.state = AutomountState::Waiting;
        inst.timeout_idle_usec = (timeout_idle_sec as u64) * 1_000_000;
        inst.directory_mode = config.directory_mode.clone();
        inst.extra_options = config.extra_options.clone();
        inst.pipe_fd = Some(pipe_fds[0]);
        inst.dev_id = dev_id;
        inst.ioctl_fd = Some(ioctl_fd);
        reg.insert(unit_name.to_string(), inst);
    }

    // Spawn pipe reader.
    let read_fd = pipe_fds[0];
    let reg_clone = registry.clone();
    let unit_name_clone = unit_name.to_string();
    let trigger_tx = mount_event_tx.clone();
    tokio::spawn(async move {
        automount_pipe_reader(read_fd, reg_clone, unit_name_clone, trigger_tx).await;
    });

    // Spawn expire timer if timeout_idle is set.
    if timeout_idle_sec > 0 {
        let reg_clone = registry.clone();
        let unit_name_clone = unit_name.to_string();
        let expire_interval = std::cmp::max(timeout_idle_sec / 3, 1);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(expire_interval as u64)).await;
                let should_expire = {
                    let reg = reg_clone.lock();
                    reg.get(&unit_name_clone)
                        .map(|inst| inst.state == AutomountState::Running)
                        .unwrap_or(false)
                };
                if should_expire {
                    if let Err(e) = do_expire(reg_clone.clone(), &unit_name_clone) {
                        warn!("Automount expire failed for {}: {}", unit_name_clone, e);
                    }
                }
            }
        });
    }

    info!("Automount waiting for {} on {}", unit_name, where_);
    Ok(())
}

pub async fn automount_enter_dead(
    registry: AutomountRegistry,
    unit_name: &str,
) -> Result<()> {
    let (where_, pipe_fd, ioctl_fd) = {
        let mut reg = registry.lock();
        let inst = match reg.get_mut(unit_name) {
            Some(i) => i,
            None => return Ok(()),
        };
        let w = inst.where_.clone();
        let pfd = inst.pipe_fd.take();
        let ifd = inst.ioctl_fd.take();
        inst.state = AutomountState::Dead;
        (w, pfd, ifd)
    };

    // Unmount autofs.
    let _ = unmount_autofs(&where_);

    // Close fds.
    if let Some(fd) = pipe_fd {
        unsafe { libc::close(fd); }
    }
    if let Some(fd) = ioctl_fd {
        unsafe { libc::close(fd); }
    }

    info!("Automount dead for {}", unit_name);
    Ok(())
}

// ---------------------------------------------------------------------------
// Pipe reader: handles kernel autofs trigger packets
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AutomountTrigger {
    pub unit_name: String,
    pub event: TriggerEvent,
}

#[derive(Debug, Clone)]
pub enum TriggerEvent {
    MountRequest { token: u32 },
    ExpireRequest { token: u32 },
}

async fn automount_pipe_reader(
    read_fd: i32,
    registry: AutomountRegistry,
    unit_name: String,
    trigger_tx: tokio::sync::mpsc::UnboundedSender<AutomountTrigger>,
) {
    let async_fd = match AsyncFd::new(read_fd) {
        Ok(fd) => fd,
        Err(e) => {
            warn!("Failed to create AsyncFd for automount pipe: {}", e);
            return;
        }
    };

    loop {
        let mut guard = match async_fd.readable().await {
            Ok(g) => g,
            Err(_) => break,
        };

        let mut packet: AutofsPacket = unsafe { std::mem::zeroed() };
        let packet_size = std::mem::size_of::<AutofsPacket>();
        let n = unsafe {
            libc::read(
                read_fd,
                &mut packet as *mut _ as *mut libc::c_void,
                packet_size,
            )
        };

        guard.retain_ready();
        drop(guard);

        if n <= 0 {
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    continue;
                }
                warn!("Error reading automount pipe for {}: {}", unit_name, err);
            }
            // EOF or error — pipe closed
            break;
        }

        let event = match packet.type_ {
            AUTOFS_PTYPE_MISSING_DIRECT => {
                debug!(
                    "Automount mount request for {} (token={})",
                    unit_name, packet.wait_queue_token
                );
                // Record token.
                {
                    let mut reg = registry.lock();
                    if let Some(inst) = reg.get_mut(&unit_name) {
                        inst.tokens.push(packet.wait_queue_token);
                        inst.state = AutomountState::Running;
                    }
                }
                TriggerEvent::MountRequest {
                    token: packet.wait_queue_token,
                }
            }
            AUTOFS_PTYPE_EXPIRE_DIRECT => {
                debug!(
                    "Automount expire request for {} (token={})",
                    unit_name, packet.wait_queue_token
                );
                {
                    let mut reg = registry.lock();
                    if let Some(inst) = reg.get_mut(&unit_name) {
                        inst.expire_tokens.push(packet.wait_queue_token);
                    }
                }
                TriggerEvent::ExpireRequest {
                    token: packet.wait_queue_token,
                }
            }
            other => {
                warn!("Unknown automount packet type {} for {}", other, unit_name);
                continue;
            }
        };

        let _ = trigger_tx.send(AutomountTrigger {
            unit_name: unit_name.clone(),
            event,
        });
    }

    // Pipe closed — mark as dead.
    let mut reg = registry.lock();
    if let Some(inst) = reg.get_mut(&unit_name) {
        inst.state = AutomountState::Dead;
        inst.pipe_fd = None;
    }
    info!("Automount pipe closed for {}", unit_name);
}

// ---------------------------------------------------------------------------
// Expire handling
// ---------------------------------------------------------------------------

fn do_expire(registry: AutomountRegistry, unit_name: &str) -> Result<()> {
    let (where_, dev_id) = {
        let reg = registry.lock();
        let inst = match reg.get(unit_name) {
            Some(i) => i,
            None => return Ok(()),
        };
        (inst.where_.clone(), inst.dev_id)
    };

    let dev_autofs_fd = ensure_dev_autofs()?;
    let ioctl_fd = open_ioctl_fd(dev_autofs_fd, &where_, dev_id)?;

    unsafe {
        let mut params: AutofsDevIoctl = Default::default();
        params.ioctlfd = ioctl_fd;

        // Try expire in a loop until EAGAIN.
        loop {
            let rc = libc::ioctl(
                dev_autofs_fd,
                AUTOFS_DEV_IOCTL_EXPIRE,
                &mut params as *mut _ as *mut libc::c_void,
            );
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EAGAIN) {
                    break; // Nothing left to expire
                }
                warn!("AUTOFS_DEV_IOCTL_EXPIRE failed for {}: {}", unit_name, err);
                break;
            }
            // If expire returned a non-zero token, the kernel will send
            // an expire_direct packet on the pipe, which we handle above.
        }

        libc::close(ioctl_fd);
    }

    Ok(())
}
