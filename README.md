# System Alphabet (systema)

**System Alphabet** is a cross-platform, event-driven system resource manager built in Rust. It is designed as a non-pid1 systemd shim: it provides a compatible systemd D-Bus interface so that existing systemd front-end tools (`systemctl`, `systemd-analyze`, etc.) work transparently, while the underlying execution engine is fully replaceable.

## Architecture

```
┌────────────────────────────────────────────────────────┐
│                   External Tooling                     │
│   (systemctl, journalctl, GNOME Settings, etc.)       │
└──────────────────────┬─────────────────────────────────┘
                       │ D-Bus (org.freedesktop.systemd1)
                       │
┌──────────────────────▼─────────────────────────────────┐
│              System A  (System Allocator)               │
│  ┌────────┐  ┌──────────┐  ┌──────────┐  ┌─────────┐  │
│  │Unit    │  │DepGraph  │  │Scheduler │  │D-Bus    │  │
│  │Loader  │  │+TopoSort │  │+Dispatch │  │Server   │  │
│  └────────┘  └──────────┘  └──────────┘  └─────────┘  │
│                                                         │
│  Desired State only — never holds actual runtime state  │
└──────────────────────┬─────────────────────────────────┘
                       │ IPC (Unix Socket + Protobuf)
                       │ /run/system-alphabet/allocator.sock
           ┌───────────┼───────────┐
           │           │           │
┌──────────▼──┐ ┌──────▼──┐ ┌─────▼──────┐
│  System S   │ │System M │ │  System T  │
│  (Service)  │ │(Mount)  │ │  (Target)  │
│             │ │Phase 3  │ │ internal   │
│fork/exec    │ └─────────┘ └────────────┘
│state machine│
│DEAD→RUNNING │
└─────────────┘
```

### Components

| Component | Binary | Role |
|-----------|--------|------|
| **System A** | `system-a` | Control plane: dependency resolution, task scheduling, D-Bus |
| **System S** | `system-s` | Service execution: fork/exec, state machine, signal handling |
| **System M** | *(Phase 3)* | Mount management |
| **System C** | *(Phase 3)* | Timer/cron management |
| **System T** | internal | Target activation (inline in System A, Phase 1) |
| **System B** | *(future)* | Boot/power management |

## Phase 1 Status

- [x] Cargo workspace with `common`, `system-a`, `system-s` crates
- [x] Protobuf IPC protocol (`proto/ipc.proto`)
- [x] IPC framing (length-delimited codec over Unix socket)
- [x] Systemd unit file parser (INI format, tested)
- [x] Dependency graph + topological sort (petgraph)
- [x] Task scheduler with dependency expansion
- [x] IPC server in System A (worker registration, task dispatch)
- [x] IPC client in System S (registration, task execution)
- [x] Service process management (fork/exec via tokio::process, SIGTERM/SIGKILL)
- [x] Service state machine (Dead → Starting → Running → Dead/Failed)
- [x] Target unit activation (inline, no external worker)
- [x] D-Bus server (`org.freedesktop.systemd1.Manager`) with:
  - `StartUnit`, `StopUnit`, `RestartUnit`, `ReloadUnit`, `TryRestartUnit`, `ReloadOrRestartUnit`
  - `GetUnit`, `LoadUnit`
  - `ListUnits`, `ListJobs`, `ListUnitFiles`
  - `Reload`, `ResetFailed`, `ResetFailedUnit`
  - Manager properties (Version, SystemState, NNames, NJobs, etc.)

## Building

```bash
# Requires: Rust 1.70+, protobuf-compiler
sudo apt-get install protobuf-compiler
cargo build --workspace
```

Note: `systema-sysm.linux` (the Linux flavor of the System M mount worker)
only does real work on Linux; on other platforms it compiles to an inert
stub binary instead of Linux-only code.

## Running

```bash
# Terminal 1: start System A (requires /run/system-alphabet/ directory)
sudo mkdir -p /run/system-alphabet
sudo target/debug/system-a

# Terminal 2: start System S
sudo target/debug/system-s

# Terminal 3: use systemctl (or busctl)
systemctl --system start sshd.service
systemctl --system status sshd.service
systemctl --system stop sshd.service
```

## IPC Protocol

All messages are wrapped in an `Envelope`:

```protobuf
message Envelope {
    uint64 request_id = 1;
    string source = 2;
    string target = 3;
    string method = 4;   // "task.dispatch", "event.publish", "worker.register"
    bytes  payload = 5;  // nested protobuf message
}
```

Frames are length-delimited (4-byte big-endian length prefix) over a Unix socket at `/run/system-alphabet/allocator.sock`.

## Unit File Search Paths

System Alphabet reads unit files from (in order):

1. `/etc/system-alphabet/`
2. `/run/system-alphabet/`
3. `/usr/local/lib/system-alphabet/`
4. `/usr/lib/system-alphabet/`
5. `/etc/systemd/system/` (compatibility)
6. `/usr/lib/systemd/system/` (compatibility)
7. `/lib/systemd/system/` (compatibility)

## Design Principles

1. **Control-plane / execution-plane separation**: System A never holds actual state. It only knows _desired_ state. Real state lives in System Workers.
2. **Single-threaded async**: Each binary uses `tokio::main(flavor = "current_thread")`. No thread-per-service.
3. **Internal IPC only**: D-Bus is exposed outward for compatibility, but System A ↔ System W communication uses a lightweight protobuf/Unix-socket protocol.
4. **Platform abstraction**: Process management is abstracted behind `start_service`/`stop_service` interfaces to support future Windows backends.