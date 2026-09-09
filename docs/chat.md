# 执行 prompt.md 的任务

**Session ID:** ses_095b0f018ffeZdhiTG5bUlPDap
**Created:** 7/16/2026, 5:43:14 PM
**Updated:** 7/16/2026, 5:45:48 PM

---

## User

阅读docs/prompt.md，执行其中的任务

---

## Assistant (Plan · DeepSeek V4 Flash Free · 4.6s)

_Thinking:_

The user wants me to read `docs/prompt.md` and execute the tasks described in it. However, I'm in Plan Mode, which means I can only read and plan - I cannot make any edits or changes. Let me first read the file to understand what tasks are required.

**Tool: read**

**Input:**
```json
{
  "filePath": "/home/chenpi11/Projects/systema/systema/docs/prompt.md"
}
```

**Output:**
```
<path>/home/chenpi11/Projects/systema/systema/docs/prompt.md</path>
<type>file</type>
<content>
1: # System Alphabet 项目需求
2: 
3: 你要实现一个全新的、跨平台的系统服务/资源管理器，项目叫 **System Alphabet**。用Rust实现
4: 
5: ## 1. 项目目标
6: 
7: 构建一个**控制面与执行面严格分离**、**事件驱动**、**跨平台**的 systemd 系统替代方案。它不是 systemd 的 fork，而是一个“非pid1”的systemd shim方案，让非systemd init的系统也能用上systemd的前端生态，它能完美支持systemd，最终能够管理服务、挂载、定时任务、目标（target）等一系列单元。
8: 
9: ## 2. 顶层架构
10: 
11: 系统由两类组件构成：
12: 
13: - **System Allocator (System A)**：全局控制面。负责任务计划、依赖解析、事务协调，并对外暴露兼容 systemd1 所有的的 D-Bus 接口（仅用于外部兼容，System Alphabet 内部不用 D-Bus）。
14: - **System Workers (System W)**：执行面。每个执行器只负责一个资源域，拥有该域的运行时状态，通过异步 IPC 接受 System A 的 Task 并上报事件。
15: 
16: 具体的组件列表及职责：
17: 
18: | 组件 | 全称 | 职责 | 拥有状态 |
19: | ------ | ------ | ------ | ------ |
20: | **System A** | System Allocator | 解析 Unit 文件，构建依赖图，拓扑排序生成 Task，派发 Task，处理事件，维护期望状态（最核心） | 仅维护 **Desired State** |
21: | **System S** | System Service | 管理服务进程的生命周期：fork/exec、重启策略、cgroup/Job Object 限制、输出捕获、看门狗 | `ServiceState` 自动机 (DEAD, STARTING, RUNNING, FAILED ...) |
22: | **System M** | System Mount | 管理文件系统挂载、自动挂载 | 挂载点状态 |
23: | **System C** | System Cron | 管理定时器（单调/日历），触发定时任务 | 定时器列表、下一次触发时间 |
24: | **System P** | System Power | 负责管理电源任务，如重启、关机、休眠、睡眠 | 电源 |
25: 
26: 注：由于 Target 的特殊性，System T 不单独作为 Worker，而是内嵌在 System A 内部，负责 Target 的激活逻辑。
27: 
28: **关键设计定理：** System A 永远不知道服务的真实 PID 或是否真的挂掉，它负责规划和”发布招标“；各 System W”接单“完成任务。
29: 
30: ## 3. 通信模型
31: 
32: 内部通信完全基于一个轻量的 **IPC/事件总线**，不依赖 D-Bus。
33: 
34: - **传输层**：  
35:   - Linux: `AF_UNIX` + `SOCK_SEQPACKET`  
36:   - Windows: Named Pipe（暂时不考虑支持它）
37: - **序列化**：Protocol Buffers (protobuf) 或 MessagePack，紧凑且跨语言。
38: - **模式**：  
39:   - **RPC**：System A 向 System W 派发 Task，执行器回报 TaskResult。  
40:   - **Pub/Sub 事件流**：执行器(W)发布事件（如 `process.exit`），System A 订阅并响应。  
41: - **连接模型**：System A 作为服务器监听知名地址（如 `/run/systema/allocator.sock`），各 System W 启动后主动连接 SysA，注册自己的能力。（也就是”我处理哪一类unit“，例如 `"type": "service"/"target"/"mount"`），然后持续**拉取 (Pull)** 属于自己的 Task。连接是长连接、全双工。
42: 
43: 协议框架定义：
44: 
45: ```protobuf
46: message Envelope {
47:     uint64 request_id = 1;
48:     string source = 2;
49:     string target = 3;
50:     string method = 4;  // e.g. "task.dispatch", "event.publish"
51:     bytes payload = 5;
52: }
53: ```
54: 
55: Task 与 Event 消息的具体结构在实现时按需定义。
56: 
57: ## 4. 线程与并发模型
58: 
59: 所有组件（System A 和各 System W）均采用**单线程异步事件循环**。在 Rust 中选用 **Tokio**，利用 `epoll`（Linux）或 IOCP（Windows）驱动。严禁为每个服务创建线程。
60: 
61: ## 5. 状态所有权严格分离
62: 
63: - **System A**：只存储期望状态（例如 `desired_state[“nginx.service”] = ACTIVE`）。
64: - **System S**：维护服务真实状态机，如 `DEAD → START_PRE → STARTING → RUNNING → … → FAILED`。System A 只能通过 TaskResult 或事件得知状态变化，绝不能直接写入执行器的状态。
65: 
66: ## 6. 平台抽象
67: 
68: 必须提供统一的 `ProcessHandle`、`MountHandle` 等平台无关接口。
69: 
70: - **Linux 实现**：使用 `fork()`、`execve()`、`pidfd`、`cgroup v2`、`signalfd`。
71: - **Windows 实现**：使用 `CreateProcessW()`、Job Object、IOCP、Windows Service API。
72: 
73: ## 7. Unit 文件格式
74: 
75: 保持与 systemd 完全相同的配置格式。使用Rust中解析systemd配置的轮子
76: 
77: ## 8. 开发路线图
78: 
79: 按以下阶段实现，每阶段产出可编译运行的代码。
80: 
81: ### Phase 1：核心调度与服务管理 [已完成]
82: 
83: - 实现 System A 的骨架：Unit Loader、依赖图构建、拓扑排序、Task 生成器。
84: - 实现 System S 骨架：状态机、进程启动/停止/监视（Linux 版本，用简单 fork/exec）。
85: - 实现两者之间的 IPC：System A 与 Sysyem S 通过 Unix Socket 通信，Task 派发与结果回报。
86: - 支持 `systemctl start/stop sshd.service`，能正确启动和停止一个简单服务。
87: - 实现 `target` 的基本激活（System T 可作为 System A 内部逻辑，暂不独立）。
88: 
89: ### Phase 2：事件总线与故障恢复
90: 
91: - 实现事件总线：pub/sub 机制，System S 发布 `process.exit`，SysA 触发 restart。
92: - 实现重启策略（on-failure, always）。
93: - System A 能正确从崩溃中恢复：重读 Unit 文件，与各 System E 恢复连接并同步状态。
94: 
95: ### Phase 3：挂载与定时任务
96: 
97: - 实现 System M 和 System C，以及对应的 `.mount`、`.timer` 单元解析。
98: - 实现自动挂载（按需挂载）逻辑。
99: - System C 支持单调时钟与日历定时器，可触发 service 启动。
100: 
101: ### Phase 4：跨平台支持（Windows）
102: 
103: - 将所有 System E 的平台抽象层实现 Windows 后端。
104: - 至少在 Windows 上能启动/停止一个守护进程（模拟服务），并进行挂载管理。
105: 
106: ## 9. AI 的工作方式
107: 
108: - 请首先输出你对整个架构的理解，确认没有歧义。
109: - 然后从 Phase 1 开始，逐步给出每个模块的详细设计和 Rust 代码骨架，包括 Cargo.toml 依赖、模块划分、关键类型定义和核心逻辑伪代码（或可直接编译的代码）。
110: - 每完成一个 Phase，等待我对当前阶段的测试反馈后再继续下一阶段。
111: - 代码风格遵循 Rust 社区惯例，注重错误处理和日志。
112: - 所有 IPC 消息使用 protobuf 定义，放在独立的 `proto/` 目录。
113: 
114: 现在，请开始你的理解和 Phase 2 的设计方案。
115: 有任何问题，用简体中文提出
116: 现在不开始写代码，而是开始制定 Phase 2 的计划

(End of file - total 116 lines)
</content>
```

---

## Assistant (Plan · DeepSeek V4 Flash Free · 102.1s)

_Thinking:_

The user wants me to read `docs/prompt.md` and execute the tasks within it. Let me analyze the document:

1. This is a project called **System Alphabet** - a cross-platform system service/resource manager built in Rust.
2. The architecture has System Allocator (System A) as control plane and System Workers (System S, System M, System C, System B) as execution plane.
3. Phase 1 is marked as completed. Phase 2 is the next step.
4. The document ends with "现在不开始写代码，而是开始制定 Phase 2 的计划" (Don't start writing code now, start making the Phase 2 plan).

So the user wants me to:
1. First, understand the current state of the codebase (what's already implemented in Phase 1)
2. Then create a plan for Phase 2 implementation

Let me explore the codebase to understand what exists.

好的，让我先了解当前代码库的 Phase 1 实现状态，然后制定 Phase 2 计划。

**Tool: task**

**Input:**
```json
{
  "description": "Explore codebase structure",
  "prompt": "Explore the codebase at /home/chenpi11/Projects/systema/systema thoroughly. I need to understand:\n1. The top-level directory structure\n2. What's in Cargo.toml (workspace members, dependencies)\n3. The module structure under src/ for each crate\n4. Key types and traits defined (especially System A allocator, System S service manager, IPC communication)\n5. Any existing proto/ directory with protobuf definitions\n6. What's implemented vs what's stubbed out\n\nReturn a detailed summary of:\n- All crates and their purposes\n- Key types, structs, enums, traits\n- The IPC implementation\n- What Phase 1 features are present and what Phase 2 features are missing\n- The full workspace Cargo.toml and each crate's Cargo.toml",
  "subagent_type": "explore"
}
```

**Output:**
```
<task id="ses_095b0ca11ffe35KDOJIM8Zz5Tr" state="completed">
<task_result>
Here is the detailed summary of the entire `systema` codebase.

---

## 1. Top-Level Directory Structure

```
/home/chenpi11/Projects/systema/systema/
├── .git/                         # Git repository
├── .gitignore
├── .marscode/
├── Cargo.lock
├── Cargo.toml                    # Workspace root
├── crates/
│   ├── common/                   # Shared library (IPC types, framing, paths)
│   ├── system-a/                 # System Allocator binary (control plane)
│   └── system-s/                 # System Service binary (service execution worker)
├── docs/
│   ├── allocator-todo.md         # Chinese TODO tracking System A features
│   ├── dbus-todo.md              # Chinese TODO tracking D-Bus interface parity
│   └── prompt.md                 # Original project requirements document
├── LICENSE
├── proto/
│   └── ipc.proto                 # Protobuf IPC definitions
├── README.md
├── target/                       # Build artifacts (gitignored)
├── termux-env-local.sh
└── termux-env.sh
```

---

## 2. Workspace Cargo.toml

**File:** `/home/chenpi11/Projects/systema/systema/Cargo.toml`

**Workspace members:** `crates/common`, `crates/system-a`, `crates/system-s`

**Workspace dependencies:**
- **Async runtime:** `tokio` (full), `tokio-util` (codec)
- **Serialization:** `prost` 0.13, `prost-types` 0.13, `bytes` 1, `serde` (derive), `serde_json`
- **D-Bus:** `zbus` 4 (tokio), `zvariant` 4
- **Graph:** `petgraph` 0.6
- **Config parsing:** `configparser` 3 (INI)
- **Error handling:** `anyhow` 1, `thiserror` 2
- **Logging:** `tracing` 0.1, `tracing-subscriber` (env-filter)
- **Utilities:** `futures` 0.3, `async-trait` 0.1, `uuid` 1 (v4), `once_cell` 1, `parking_lot` 0.12

---

## 3. Crate-by-Crate Breakdown

### 3.1 `common` (Shared Library)

**`/home/chenpi11/Projects/systema/systema/crates/common/Cargo.toml`**

- **Purpose:** Shared library used by both System A and System Workers. Provides:
  - Protobuf-generated IPC message types
  - IPC framing utilities (length-delimited codec over Unix sockets)
  - Compile-time configurable filesystem paths

**Module structure:**
```
common/src/
├── lib.rs       # Re-exports ipc, paths, proto modules
├── ipc.rs       # IPC framing: EnvelopeFramed, send_envelope, recv_envelope, make_envelope
├── build.rs     # Compiles proto/ipc.proto via prost-build, generates paths.rs
```

**Key types:**
- `EnvelopeFramed` -- `Framed<UnixStream, LengthDelimitedCodec>` type alias
- Functions in `ipc.rs`: `frame_stream()`, `send_envelope()`, `recv_envelope()`, `make_envelope()`

**Proto-generated types** (via `proto::` module):
- All messages from `ipc.proto`: `Envelope`, `WorkerRegistration`, `TaskDispatch`, `TaskResult`, `EventPublish`, `RegisterAck`, `UnitConfig`, `ServiceConfig`, enums `TaskKind`, `TaskResultKind`

**Paths** (generated at build time, configurable via env vars):
- `SYSTEMA_ETC_DIR`, `SYSTEMA_RUN_DIR`, `SYSTEMA_LOCAL_LIB_DIR`, `SYSTEMA_LIB_DIR`
- `SYSTEMD_ETC_UNIT_DIR`, `SYSTEMD_LIB_UNIT_DIR`, `SYSTEMD_ALT_UNIT_DIR`
- `IPC_SOCKET_PATH` (default: `/run/systema/allocator.sock`)
- `UNIT_SEARCH_PATHS`, `GENERATOR_SEARCH_PATHS`

---

### 3.2 `system-a` (System Allocator)

**`/home/chenpi11/Projects/systema/systema/crates/system-a/Cargo.toml`**

- **Purpose:** Global control plane. Loads unit files, builds dependency graph, schedules and dispatches tasks to workers via IPC, exposes systemd-compatible D-Bus interface. Maintains desired state only.

**Module structure:**
```
system-a/src/
├── main.rs         # Entry point: starts IPC server + D-Bus server concurrently
├── state.rs        # AllocatorState, AllocatorHandle, types for jobs/workers/units
├── unit/
│   ├── mod.rs      # Re-exports
│   ├── types.rs    # UnitFile, UnitSection, ServiceSection, ExecCommand, etc.
│   ├── parser.rs   # INI-based systemd unit file parser
│   └── loader.rs   # Unit file discovery and loading from search paths
├── graph/
│   └── mod.rs      # DependencyGraph with petgraph, topological sort, cycle-breaking
├── scheduler/
│   └── mod.rs      # Job enqueueing, dependency expansion, task dispatch, result handling
├── ipc/
│   ├── mod.rs
│   └── server.rs   # Unix socket IPC server: worker registration, task dispatch, event handling
└── dbus/
    ├── mod.rs      # D-Bus server setup, per-unit object registration
    ├── manager.rs  # org.freedesktop.systemd1.Manager interface
    ├── properties.rs  # Custom org.freedesktop.DBus.Properties (accepts empty interface)
    ├── unit_obj.rs    # org.freedesktop.systemd1.Unit per-unit interface
    ├── service_obj.rs # org.freedesktop.systemd1.Service interface
    ├── socket_obj.rs  # org.freedesktop.systemd1.Socket interface (stub)
    └── slice_obj.rs   # org.freedesktop.systemd1.Slice interface (stub)
```

**Key types defined in `state.rs`:**
- `DesiredState` -- `Active | Inactive`
- `JobKind` -- `Start | Stop | Restart | Reload`
- `JobMode` -- `Replace | Fail | Queue | Isolate | Flush | IgnoreDependencies | IgnoreRequirements`
- `JobStatus` -- `Waiting | Running | Done | Failed(String) | Cancelled`
- `ActiveState` -- `Inactive | Activating | Active | Deactivating | Failed | Reloading`
- `AllocatorState` -- Full mutable state: units map, desired map, runtime map, jobs map, workers map, task_kinds map, channels for D-Bus signals
- `AllocatorHandle` -- `Arc<RwLock<AllocatorState>>`
- `Allocator` -- Builder struct with `new()` that returns `AllocatorHandle`
- `WorkerEntry` -- Registered worker with channel for tasks
- `WorkerTask` -- Task sent to a worker over mpsc channel
- `Job`, `JobResult`, `JobCompletion`, `JobNewInfo`, `UnitRuntimeInfo`, `StartLimitState`
- ID generators: `next_job_id()`, `next_task_id()`, `next_request_id()`

**Key types in `unit/types.rs`:**
- `UnitKind` -- `Service | Target | Mount | Timer | Socket | Slice | Scope | Swap | Path | Device | Unknown(String)`
- `UnitFile` -- Complete parsed unit with sections: `unit`, `install`, `service`, `mount`, `timer`, `socket`, `swap`, `path`, `slice`, `scope`, `device`
- `UnitSection` -- `[Unit]` with all dependency fields (Requires, Wants, After, Before, BindsTo, PartOf, Requisite, Upholds, OnSuccess, OnFailure, PropagatesReloadTo, Conflicts) and Condition/Assert fields
- `ServiceSection` -- `[Service]` with Type, ExecStart/Stop/Reload, Restart policy, timeouts, environment, etc.
- `ExecCommand` -- Parsed exec line with prefixes (`-`, `+`, `@`, `:`, `!`, `!!`)
- `ServiceType` -- `Simple | Forking | Oneshot | Dbus | Notify | NotifyReload | Idle`
- `RestartPolicy` -- `No | OnSuccess | OnFailure | OnAbnormal | OnWatchdog | OnAbort | Always`
- `InstallSection`, `MountSection`, `TimerSection`, `SocketSection`, `SwapSection`, `PathSection`, `SliceSection`, `ScopeSection`, `DeviceSection`

**Key types in `graph/mod.rs`:**
- `EdgeKind` -- `Strong | Weak`
- `DependencyGraph` -- Wraps `petgraph::DiGraph<String, EdgeKind>`, provides `build()`, `topological_order()`, `start_order_for()`, `required_deps()`

**Key types in `scheduler/mod.rs`:**
- `PostAction` (private enum) -- `Stop(String) | Start(String)` -- for post-task-result cascading
- Public functions: `enqueue_job()`, `enqueue_start()`, `enqueue_stop()`, `enqueue_restart()`, `enqueue_start_with_mode()`, `handle_task_result()`, `build_task_dispatch()`

**D-Bus interfaces implemented:**
- `org.freedesktop.systemd1.Manager` -- Full methods: `GetUnit`, `LoadUnit`, `StartUnit`, `StopUnit`, `RestartUnit`, `ReloadUnit`, `TryRestartUnit`, `ReloadOrRestartUnit`, `ListUnits`, `ListUnitsFiltered`, `ListUnitsByPatterns`, `ListUnitsByNames`, `ListJobs`, `ListUnitFiles`, `ListUnitFilesByPatterns`, `GetUnitFileState`, `GetUnitProcesses`, `Reload`, `ResetFailedUnit`, `ResetFailed`; Signals: `JobNew`, `JobRemoved`; Properties: Version, Features, Architecture, NNames, NFailedUnits, NJobs, Environment, UnitPath, SystemState, etc.
- `org.freedesktop.systemd1.Unit` -- Properties: Id, Names, Description, LoadState, ActiveState, SubState, UnitFileState, FragmentPath, CanStart/Stop/Reload, Requires, Wants, After, Job, etc.; Method: `ResetFailed`
- `org.freedesktop.systemd1.Service` -- Properties: Type, MainPID, BusName, Restart, NotifyAccess, Result, etc.
- `org.freedesktop.systemd1.Socket` -- Stub with Result, NAccepted, NConnections, ControlPID (all fixed)
- `org.freedesktop.systemd1.Slice` -- Empty interface stub
- Custom `org.freedesktop.DBus.Properties` -- Handles empty interface name (systemd extension)

---

### 3.3 `system-s` (System Service)

**`/home/chenpi11/Projects/systema/systema/crates/system-s/Cargo.toml`**

- **Purpose:** Service execution worker. Connects to System A, registers as a "service" worker, receives `TaskDispatch` messages, executes service processes via fork/exec, reports results and events.
- **Unix-only dependency:** `nix` 0.29 (process, signal, user) for signal delivery

**Module structure:**
```
system-s/src/
├── main.rs      # Entry point: initializes logging, runs IPC client
├── state.rs     # ServiceState machine, ServiceInstance, ServiceRegistry
├── process.rs   # Process management: start_service, stop_service, wait_for_child, is_alive
└── ipc/
    └── mod.rs   # IPC client: connect to System A, register, handle tasks, monitor services
```

**Key types in `state.rs`:**
- `ServiceState` -- `Dead | StartPre | Starting | Running | Reloading | Stopping | StopPost | Failed`
- `ServiceInstance` -- Per-service runtime state: unit_name, state, main_pid, n_restarts, last_exit_code
- `ServiceRegistry` -- `Arc<Mutex<HashMap<String, ServiceInstance>>>`

**Key types/functions in `process.rs`:**
- `start_service()` -- Parses ExecStart, spawns child via `tokio::process::Command`, returns (pid, Child)
- `stop_service()` -- Sends SIGTERM, polls until exit, sends SIGKILL after timeout
- `wait_for_child()` -- Awaits child exit, returns (exit_code, success)
- `is_alive()` -- Unix-only check with `kill(pid, None)`
- Helper: `parse_exec_cmd()` -- Strips systemd prefixes, shell-splits command line

**Key functions in `ipc/mod.rs`:**
- `run()` -- Main entry, connects to System A, retries with exponential backoff
- `try_run()` -- Registration handshake, splits connection, runs reader+writer tasks
- `execute_task()` -- Handles Start/Stop/Restart/Reload task kinds
- `monitor_service()` -- Background task that polls child every second, reports `service.failed` on non-zero exit
- `queue_event()` -- Sends `EventPublish` messages (service.started, service.failed, process.exit)

---

## 4. IPC Implementation

### Protocol (`proto/ipc.proto`)

The IPC protocol is defined in **`/home/chenpi11/Projects/systema/systema/proto/ipc.proto`** (108 lines).

**Messages:**
- `Envelope` -- Universal wrapper: `request_id`, `source`, `target`, `method`, `payload` (bytes)
- `WorkerRegistration` -- Sent by worker on connect: `worker_id`, `unit_types`
- `TaskDispatch` -- Sent by System A to worker: `task_id`, `unit_name`, `unit_type`, `kind` (TaskKind), `unit_config`
- `TaskResult` -- Sent by worker to System A: `task_id`, `success`, `message`, `result_kind`, `unit_name`
- `EventPublish` -- Sent by worker: `event_type`, `unit_name`, `event_data`
- `RegisterAck` -- Sent by System A: `accepted`, `message`
- `UnitConfig` / `ServiceConfig` -- Unit configuration sent with task dispatch

**Enums:**
- `TaskKind` -- `TASK_KIND_START=0 | STOP=1 | RESTART=2 | RELOAD=3`
- `TaskResultKind` -- `TASK_RESULT_DONE=0 | FAILED=1 | TIMEOUT=2 | SKIPPED=3`

### Transport

- **Socket:** Unix domain socket at `/run/systema/allocator.sock` (configurable via `SYSTEMA_IPC_SOCKET` env var)
- **Framing:** 4-byte big-endian length prefix + protobuf-encoded `Envelope` bytes, using `tokio_util::codec::LengthDelimitedCodec` with 16 MiB max frame
- **Connection model:** System A listens as server. Workers connect, register, then full-duplex communication via `tokio::io::split` (separate `FramedRead`/`FramedWrite`)
- **Methods used:** `worker.register` / `worker.ack`, `task.dispatch` (A→W), `task.result` (W→A), `event.publish` (W→A)

### Flow

1. Worker connects → sends `WorkerRegistration` (e.g., `worker_id="system-s-1"`, `unit_types=["service"]`)
2. System A acknowledges with `RegisterAck`
3. System A creates an `mpsc::channel` for the worker and inserts a `WorkerEntry` in `AllocatorState.workers`
4. Sender task: reads from channel, builds `TaskDispatch`, writes to socket
5. Receiver task: reads from socket, dispatches via `dispatch_incoming()` which handles `task.result` (→ `handle_task_result`) and `event.publish` (→ `handle_event`)
6. On disconnect: worker is deregistered, pending jobs are cancelled, transitional units are marked Failed

---

## 5. What's Implemented (Phase 1) vs Missing (Phase 2+)

### Fully Implemented (Phase 1)

| Feature | Details |
|---------|---------|
| **Workspace/crate structure** | 3 crates: `common`, `system-a`, `system-s` |
| **Protobuf IPC protocol** | `proto/ipc.proto` with all messages |
| **IPC framing** | Length-delimited codec over Unix socket |
| **Unit file parser** | Full INI parser supporting all 10 unit types, drop-ins, specifiers, exec prefixes, conditions/asserts |
| **Dependency graph** | petgraph-based DAG with After/Before/Requires/Wants/BindsTo/Requisite/Upholds/Conflicts/OnSuccess/OnFailure/PropagatesReloadTo/PartOf |
| **Topological sort** | With cycle-breaking (weak edges first) |
| **Task scheduler** | Dependency expansion, job conflict detection, 7 job modes, serial task chaining, timeouts, restart policy, rate limiting |
| **IPC server (System A)** | Worker registration, task dispatch, result/event handling, disconnect cleanup |
| **IPC client (System S)** | Registration, task execution (Start/Stop/Restart/Reload), background process monitoring |
| **Service process management** | fork/exec via tokio, SIGTERM→SIGKILL stop, process monitoring |
| **Target activation** | Inline in System A, no external worker needed |
| **D-Bus Manager** | All major methods (StartUnit, StopUnit, ListUnits, etc.), JobNew/JobRemoved signals, Manager properties |
| **D-Bus Unit/Service objects** | Per-unit D-Bus objects with properties |
| **Unit loading** | Recursive directory scanning, on-demand loading, transient units, masking detection, aliases |
| **Tests** | Extensive unit tests: parser (~33), graph (~30), scheduler (~34), types (~17), loader (~4) |

### Missing or Stubbed (Phase 2+)

| Feature | Phase | Status |
|---------|-------|--------|
| **Event bus pub/sub** | Phase 2 | Only System A processes events; no general publish-subscribe |
| **System A crash recovery** | Phase 2 | No re-sync with workers after restart |
| **State reconciliation loop** | Phase 2 | No continuous desired-vs-actual reconciliation |
| **InvocationID** | Phase 2 | Not implemented |
| **Unit reference counting** | Phase 2 | `RefUnit`/`UnrefUnit` not implemented |
| **SOCK_SEQPACKET** | Phase 2 | Currently uses SOCK_STREAM via `UnixListener` |
| **Load balancing** | Phase 2 | Only picks first matching worker |
| **Backpressure** | Phase 2 | Task channel is bounded (64) but no backpressure signaling |
| **Multiple workers of same type** | Phase 2 | Not supported |
| **Worker reconnection** | Phase 2 | System A side doesn't attempt reconnection |
| **System M (Mount)** | Phase 3 | Crate doesn't exist |
| **System C (Cron/Timer)** | Phase 3 | Crate doesn't exist |
| **System P (Power)** | ✅ 已实现 | `systema-sysp.linux`（libc `reboot(2)`）与 `systema-sysp.shim`（no-op）共享 `worker_ipc` 接口；目前仅 Start 触发电源操作 |
| **D-Bus: many methods** | Phase 2+ | ~50 manager methods missing (CancelJob, KillUnit, FreezeUnit, EnableUnitFiles, etc.) |
| **D-Bus: Job object** | Phase 2 | Job paths returned but no registered Job D-Bus objects |
| **D-Bus: Target/Mount/Timer/Socket/Swap/Path/Scope/Device interfaces** | Phase 2+ | All unimplemented (stubs for Socket/Slice only) |
| **D-Bus: PropertiesChanged signal** | Phase 2 | Not emitted |
| **D-Bus: all Manager properties** | Phase 2 | ~20 CLR/RLIMIT properties missing, all timestamps return 0 |
| **Platform abstraction** | Phase 4 | No `ProcessHandle` trait; nix used directly on Linux |
| **Windows support** | Phase 4 | None |
| **inotify file watching** | Phase 2 | Not planned per docs |

---

## 6. Test Coverage Summary

Total test functions found: ~100+ across the codebase.

| Crate | Module | Test count | Coverage |
|-------|--------|------------|----------|
| **system-a** | `unit/types.rs` | ~17 | ExecCommand parsing, UnitKind classification, shell_words |
| **system-a** | `unit/parser.rs` | ~33 | All unit types parsing, conditions, asserts, drop-ins, specifiers |
| **system-a** | `unit/loader.rs` | ~4 | Extension check, load/unload/transient |
| **system-a** | `graph/mod.rs` | ~30 | Build, topological sort, cycle-breaking, start_order_for, required_deps |
| **system-a** | `scheduler/mod.rs` | ~34 | Job modes, start/stop limits, start/stop/reload order, conditions, asserts, glob matching, task dispatch |

Missing tests: IPC integration, D-Bus integration, end-to-end (systemctl), system-s unit tests.

---

## 7. All Cargo.toml Files

Already shown in full above -- the workspace root, common, system-a, and system-s Cargo.toml files are complete and all dependencies are workspace-shared.

---

## 8. Key Architectural Observations

1. **Strict state separation:** System A holds only desired state + cached runtime state (from worker reports). System S holds the real process state machine.
2. **Single-threaded:** Both binaries use `tokio::main(flavor = "current_thread")`.
3. **Lock discipline:** `parking_lot::RwLock` is used, never held across `.await` points.
4. **D-Bus compatibility:** Claims `org.freedesktop.systemd1` bus name, version `"255"`, and supports enough of the Manager/Unit/Service interfaces for `systemctl` to work.
5. **Code quality:** Heavy use of `anyhow` for error handling, `tracing` for logging, extensive unit tests, idiomatic Rust with async/await.
</task_result>
</task>
```

---

## Assistant (Plan · DeepSeek V4 Flash Free · 5.0s)

_Thinking:_

Now I have a comprehensive understanding of the codebase. Let me review what Phase 2 requires according to `docs/prompt.md`:

### Phase 2：事件总线与故障恢复

1. **事件总线：pub/sub 机制**，System S 发布 `process.exit`，SysA 触发 restart。
2. **重启策略（on-failure, always）**。
3. **System A 能正确从崩溃中恢复**：重读 Unit 文件，与各 System E 恢复连接并同步状态。

Now, looking at what's currently implemented:
- System S already publishes events (`service.started`, `service.failed`, `process.exit`) 
- System A already receives events via `handle_event()`
- System A already has restart policy support in the scheduler (based on the extensive scheduler tests)
- But there's no proper pub/sub event bus mechanism
- Crash recovery is not implemented
- No continuous desired-vs-actual reconciliation loop

Let me also check the existing event handling and restart policy implementations more closely.

Actually, wait - the agent already provided a very detailed analysis. Let me look at the specific Phase 2 items more carefully by checking the relevant source files.

Let me look at what's exactly there for event handling and scheduler restart policies.

让我深入了解现有的事件处理和重启策略实现细节。

**Tool: read**

**Input:**
```json
{
  "filePath": "/home/chenpi11/Projects/systema/systema/crates/system-a/src/ipc/server.rs"
}
```

**Output:**
```
<path>/home/chenpi11/Projects/systema/systema/crates/system-a/src/ipc/server.rs</path>
<type>file</type>
<content>
1: //! IPC server for System A.
2: //!
3: //! Listens on a Unix socket at `SOCKET_PATH`. Each System Worker connects,
4: //! sends a `WorkerRegistration`, then receives dispatched `TaskDispatch`
5: //! messages and sends back `TaskResult` / `EventPublish` messages.
6: 
7: use anyhow::Result;
8: use prost::Message as ProstMessage;
9: use tokio::net::UnixListener;
10: use tokio::sync::mpsc;
11: use tracing::{debug, error, info, warn};
12: 
13: use common::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
14: use common::proto::{Envelope, EventPublish, RegisterAck, TaskResult, WorkerRegistration};
15: 
16: use crate::scheduler::{self, build_task_dispatch};
17: use crate::state::{
18:     next_request_id, ActiveState, AllocatorHandle, JobCompletion, JobKind, JobMode, JobResultKind,
19:     JobStatus, WorkerEntry, WorkerTask,
20: };
21: use crate::unit::types::RestartPolicy;
22: 
23: pub const SOCKET_PATH: &str = common::paths::IPC_SOCKET_PATH;
24: 
25: /// Run the IPC server — accepts System Worker connections indefinitely.
26: pub async fn run(allocator: AllocatorHandle) -> Result<()> {
27:     // Ensure the socket directory exists.
28:     if let Some(parent) = std::path::Path::new(SOCKET_PATH).parent() {
29:         tokio::fs::create_dir_all(parent).await?;
30:     }
31: 
32:     // Remove stale socket file.
33:     let _ = tokio::fs::remove_file(SOCKET_PATH).await;
34: 
35:     let listener = UnixListener::bind(SOCKET_PATH)?;
36:     info!("IPC server listening on {}", SOCKET_PATH);
37: 
38:     loop {
39:         match listener.accept().await {
40:             Ok((stream, _addr)) => {
41:                 let alloc = allocator.clone();
42:                 tokio::spawn(async move {
43:                     if let Err(e) = handle_worker(stream, alloc).await {
44:                         error!("Worker connection error: {}", e);
45:                     }
46:                 });
47:             }
48:             Err(e) => {
49:                 error!("Accept error: {}", e);
50:             }
51:         }
52:     }
53: }
54: 
55: /// Handle a single worker connection from registration through task dispatch.
56: async fn handle_worker(stream: tokio::net::UnixStream, allocator: AllocatorHandle) -> Result<()> {
57:     let mut framed = frame_stream(stream);
58: 
59:     // --- Step 1: receive WorkerRegistration ---
60:     let env = recv_envelope(&mut framed)
61:         .await?
62:         .ok_or_else(|| anyhow::anyhow!("Worker disconnected before registration"))?;
63: 
64:     if env.method != "worker.register" {
65:         anyhow::bail!("Expected 'worker.register', got '{}'", env.method);
66:     }
67: 
68:     let reg = WorkerRegistration::decode(env.payload.as_slice())?;
69:     let worker_id = reg.worker_id.clone();
70:     let unit_types = reg.unit_types.clone();
71: 
72:     info!(
73:         "Worker '{}' registered, handles: {:?}",
74:         worker_id, unit_types
75:     );
76: 
77:     // --- Step 2: send RegisterAck ---
78:     let ack = RegisterAck {
79:         accepted: true,
80:         message: "Welcome".to_string(),
81:     };
82:     let ack_env = make_envelope(next_request_id(), "system-a", &worker_id, "worker.ack", ack)?;
83:     send_envelope(&mut framed, &ack_env).await?;
84: 
85:     // --- Step 3: create task channel and register worker ---
86:     let (task_tx, mut task_rx) = mpsc::channel::<WorkerTask>(64);
87:     {
88:         let mut state = allocator.write();
89:         state.workers.insert(
90:             worker_id.clone(),
91:             WorkerEntry {
92:                 worker_id: worker_id.clone(),
93:                 unit_types: unit_types.clone(),
94:                 task_tx,
95:             },
96:         );
97:     }
98: 
99:     // We need to both send tasks and receive results/events over the same
100:     // connection. Split the framed into two halves.
101:     let inner = framed.into_inner();
102:     let (reader, writer) = tokio::io::split(inner);
103:     let writer_stream = {
104:         use tokio_util::codec::LengthDelimitedCodec;
105:         let codec = LengthDelimitedCodec::builder()
106:             .max_frame_length(16 * 1024 * 1024)
107:             .new_codec();
108:         tokio_util::codec::FramedWrite::new(writer, codec)
109:     };
110:     let reader_stream = {
111:         use tokio_util::codec::LengthDelimitedCodec;
112:         let codec = LengthDelimitedCodec::builder()
113:             .max_frame_length(16 * 1024 * 1024)
114:             .new_codec();
115:         tokio_util::codec::FramedRead::new(reader, codec)
116:     };
117: 
118:     let alloc_for_recv = allocator.clone();
119:     let worker_id_recv = worker_id.clone();
120:     let worker_id_send = worker_id.clone();
121: 
122:     // Sender task: take tasks from channel and write to socket.
123:     let sender = async move {
124:         use futures::SinkExt;
125:         let mut writer = writer_stream;
126:         while let Some(task) = task_rx.recv().await {
127:             let dispatch = build_task_dispatch(&task);
128:             let env = make_envelope(
129:                 next_request_id(),
130:                 "system-a",
131:                 &worker_id_send,
132:                 "task.dispatch",
133:                 dispatch,
134:             )?;
135:             let mut buf = bytes::BytesMut::new();
136:             env.encode(&mut buf)?;
137:             writer.send(buf.freeze()).await?;
138:         }
139:         Ok::<_, anyhow::Error>(())
140:     };
141: 
142:     // Receiver task: read results/events from socket and update state.
143:     let receiver = async move {
144:         use futures::StreamExt;
145:         let mut reader = reader_stream;
146:         loop {
147:             match reader.next().await {
148:                 None => {
149:                     info!("Worker '{}' disconnected", worker_id_recv);
150:                     break;
151:                 }
152:                 Some(Err(e)) => {
153:                     warn!("Worker '{}' read error: {}", worker_id_recv, e);
154:                     break;
155:                 }
156:                 Some(Ok(bytes)) => {
157:                     let env = match Envelope::decode(bytes.freeze()) {
158:                         Ok(e) => e,
159:                         Err(e) => {
160:                             warn!(
161:                                 "Failed to decode envelope from worker '{}': {}",
162:                                 worker_id_recv, e
163:                             );
164:                             continue;
165:                         }
166:                     };
167:                     debug!(
168:                         "IPC envelope received from worker '{}': method={} source={} target={} payload_len={}",
169:                         worker_id_recv, env.method, env.source, env.target, env.payload.len()
170:                     );
171:                     if let Err(e) = dispatch_incoming(env, alloc_for_recv.clone()).await {
172:                         warn!("Error handling worker message: {}", e);
173:                     }
174:                 }
175:             }
176:         }
177:         Ok::<_, anyhow::Error>(())
178:     };
179: 
180:     // Run sender and receiver concurrently.
181:     tokio::select! {
182:         res = sender => {
183:             if let Err(e) = res { warn!("Sender error for worker: {}", e); }
184:         }
185:         res = receiver => {
186:             if let Err(e) = res { warn!("Receiver error for worker: {}", e); }
187:         }
188:     }
189: 
190:     // Deregister the worker and cancel any jobs that were pending for it.
191:     // Without this, Running/Waiting jobs would stay stuck forever if the worker
192:     // crashes or disconnects before sending a task.result back.
193:     {
194:         let mut state = allocator.write();
195:         state.workers.remove(&worker_id);
196: 
197:         // Collect all Running/Waiting jobs and mark them Cancelled.
198:         let stale: Vec<JobCompletion> = state
199:             .jobs
200:             .values_mut()
201:             .filter(|j| matches!(j.status, JobStatus::Running | JobStatus::Waiting))
202:             .map(|j| {
203:                 j.status = JobStatus::Cancelled;
204:                 JobCompletion {
205:                     job_id: j.id,
206:                     unit_name: j.unit_name.clone(),
207:                     result: JobResultKind::Cancelled,
208:                 }
209:             })
210:             .collect();
211: 
212:         // Reset any units that were in a transitional state.
213:         for rt in state.runtime.values_mut() {
214:             if matches!(
215:                 rt.active_state,
216:                 ActiveState::Activating | ActiveState::Deactivating
217:             ) {
218:                 rt.active_state = ActiveState::Failed;
219:                 rt.sub_state = "failed".to_string();
220:             }
221:         }
222: 
223:         // Emit JobRemoved for each cancelled job so waiting clients unblock.
224:         if let Some(ref tx) = state.job_completion_tx {
225:             for completion in stale {
226:                 let _ = tx.send(completion);
227:             }
228:         }
229:     }
230:     info!("Worker '{}' deregistered", worker_id);
231: 
232:     Ok(())
233: }
234: 
235: /// Dispatch an incoming envelope from a worker (task result or event).
236: async fn dispatch_incoming(env: Envelope, allocator: AllocatorHandle) -> Result<()> {
237:     match env.method.as_str() {
238:         "task.result" => {
239:             let result = TaskResult::decode(env.payload.as_slice())?;
240:             debug!(
241:                 "IPC task.result: task_id={} unit={} success={} message={:?}",
242:                 result.task_id, result.unit_name, result.success, result.message
243:             );
244:             let kind = parse_task_kind_from_context(&allocator, result.task_id);
245:             scheduler::handle_task_result(
246:                 allocator,
247:                 result.task_id,
248:                 result.success,
249:                 &result.message,
250:                 &result.unit_name,
251:                 kind,
252:             );
253:         }
254:         "event.publish" => {
255:             let event = EventPublish::decode(env.payload.as_slice())?;
256:             debug!(
257:                 "IPC event.publish: event_type={} unit={} data_len={}",
258:                 event.event_type,
259:                 event.unit_name,
260:                 event.event_data.len()
261:             );
262:             handle_event(allocator, event).await?;
263:         }
264:         other => {
265:             warn!("Unknown method from worker: {}", other);
266:         }
267:     }
268:     Ok(())
269: }
270: 
271: /// Look up what kind of task a task_id corresponds to using the tracked mapping.
272: fn parse_task_kind_from_context(allocator: &AllocatorHandle, task_id: u64) -> JobKind {
273:     allocator
274:         .read()
275:         .task_kinds
276:         .get(&task_id)
277:         .copied()
278:         .unwrap_or(JobKind::Start)
279: }
280: 
281: /// Handle an event published by a worker.
282: async fn handle_event(allocator: AllocatorHandle, event: EventPublish) -> Result<()> {
283:     info!(
284:         "Event from worker: type={}, unit={}",
285:         event.event_type, event.unit_name
286:     );
287: 
288:     match event.event_type.as_str() {
289:         "process.exit" => {
290:             let unit_name = event.unit_name.clone();
291:             let mut state = allocator.write();
292:             let rt = state.runtime.entry(unit_name.clone()).or_default();
293:             let was_active = rt.active_state == crate::state::ActiveState::Active;
294:             if was_active {
295:                 rt.active_state = crate::state::ActiveState::Inactive;
296:                 rt.sub_state = "dead".to_string();
297:             }
298:             rt.main_pid = None;
299: 
300:             // Check restart policy and trigger restart if applicable.
301:             let should_restart = was_active
302:                 && state
303:                     .units
304:                     .get(&unit_name)
305:                     .and_then(|u| u.service.as_ref())
306:                     .map(|svc| match svc.restart {
307:                         RestartPolicy::Always => true,
308:                         RestartPolicy::OnFailure
309:                         | RestartPolicy::OnAbnormal
310:                         | RestartPolicy::OnAbort
311:                         | RestartPolicy::OnWatchdog => true,
312:                         RestartPolicy::OnSuccess => {
313:                             // Only restart if the process exited with code 0
314:                             serde_json::from_slice::<serde_json::Value>(&event.event_data)
315:                                 .ok()
316:                                 .and_then(|v| v["exit_code"].as_i64())
317:                                 .map(|code| code == 0)
318:                                 .unwrap_or(false)
319:                         }
320:                         RestartPolicy::No => false,
321:                     })
322:                     .unwrap_or(false);
323: 
324:             if should_restart {
325:                 let restart_sec = state
326:                     .units
327:                     .get(&unit_name)
328:                     .and_then(|u| u.service.as_ref())
329:                     .map(|s| s.restart_sec as u64)
330:                     .unwrap_or(0);
331: 
332:                 // Rate limiting
333:                 let interval = std::time::Duration::from_secs(10);
334:                 let burst = 5u32;
335:                 let rate_ok = state
336:                     .start_limit_state
337:                     .entry(unit_name.clone())
338:                     .or_insert_with(crate::state::StartLimitState::new)
339:                     .check_rate_limit(interval, burst);
340: 
341:                 if rate_ok {
342:                     let alloc_clone = allocator.clone();
343:                     tokio::spawn(async move {
344:                         if restart_sec > 0 {
345:                             tokio::time::sleep(tokio::time::Duration::from_secs(restart_sec)).await;
346:                         }
347:                         info!("Auto-restarting {} after process.exit", unit_name);
348:                         if let Err(e) = scheduler::enqueue_job(
349:                             alloc_clone,
350:                             &unit_name,
351:                             JobKind::Start,
352:                             JobMode::Replace,
353:                         )
354:                         .await
355:                         {
356:                             warn!("Failed to auto-restart {}: {}", unit_name, e);
357:                         }
358:                     });
359:                 } else {
360:                     warn!(
361:                         "Restart rate limit exceeded for {}, not auto-restarting",
362:                         unit_name
363:                     );
364:                 }
365:             }
366:         }
367:         "service.started" => {
368:             let pid = serde_json::from_slice::<serde_json::Value>(&event.event_data)
369:                 .ok()
370:                 .and_then(|v| v["pid"].as_u64())
371:                 .map(|p| p as u32);
372:             let mut state = allocator.write();
373:             let rt = state.runtime.entry(event.unit_name.clone()).or_default();
374:             rt.active_state = crate::state::ActiveState::Active;
375:             rt.sub_state = "running".to_string();
376:             rt.main_pid = pid;
377:         }
378:         "service.failed" => {
379:             let mut state = allocator.write();
380:             let rt = state.runtime.entry(event.unit_name.clone()).or_default();
381:             rt.active_state = crate::state::ActiveState::Failed;
382:             rt.sub_state = "failed".to_string();
383:             rt.main_pid = None;
384:         }
385:         _ => {}
386:     }
387: 
388:     Ok(())
389: }

(End of file - total 389 lines)
</content>
```

**Tool: read**

**Input:**
```json
{
  "filePath": "/home/chenpi11/Projects/systema/systema/crates/system-a/src/scheduler/mod.rs"
}
```

**Output:**
```
<path>/home/chenpi11/Projects/systema/systema/crates/system-a/src/scheduler/mod.rs</path>
<type>file</type>
<content>
1: //! Task scheduler — generates and dispatches tasks to System Workers.
2: //!
3: //! When System A receives a request to start or stop a unit, the scheduler:
4: //! 1. Determines which units must be started/stopped (dependency expansion).
5: //! 2. Creates Job records for each operation.
6: //! 3. Dispatches WorkerTask messages to the appropriate workers.
7: 
8: use std::time::Duration;
9: 
10: use anyhow::{bail, Result};
11: use prost::Message;
12: use tracing::{debug, info, warn};
13: 
14: use crate::state::{
15:     next_job_id, next_task_id, ActiveState, AllocatorHandle, AllocatorState, Job, JobCompletion,
16:     JobKind, JobMode, JobNewInfo, JobResult, JobResultKind, JobStatus, StartLimitState,
17:     UnitRuntimeInfo, WorkerTask,
18: };
19: use crate::unit::types::{RestartPolicy, UnitFile, UnitKind, UnitSection};
20: use common::proto::{ServiceConfig, TaskDispatch, TaskKind, UnitConfig};
21: 
22: /// Enqueue a start job for the named unit, expanding dependencies.
23: /// Returns the primary job ID.
24: pub async fn enqueue_start(allocator: AllocatorHandle, unit_name: &str) -> Result<u64> {
25:     enqueue_job(allocator, unit_name, JobKind::Start, JobMode::Replace).await
26: }
27: 
28: /// Enqueue a stop job for the named unit.
29: pub async fn enqueue_stop(allocator: AllocatorHandle, unit_name: &str) -> Result<u64> {
30:     enqueue_job(allocator, unit_name, JobKind::Stop, JobMode::Replace).await
31: }
32: 
33: /// Enqueue a restart job for the named unit.
34: pub async fn enqueue_restart(allocator: AllocatorHandle, unit_name: &str) -> Result<u64> {
35:     enqueue_job(allocator, unit_name, JobKind::Restart, JobMode::Replace).await
36: }
37: 
38: /// Enqueue a start job with explicit mode.
39: pub async fn enqueue_start_with_mode(
40:     allocator: AllocatorHandle,
41:     unit_name: &str,
42:     mode: JobMode,
43: ) -> Result<u64> {
44:     enqueue_job(allocator, unit_name, JobKind::Start, mode).await
45: }
46: 
47: /// Core job enqueueing logic.
48: pub async fn enqueue_job(
49:     allocator: AllocatorHandle,
50:     unit_name: &str,
51:     kind: JobKind,
52:     mode: JobMode,
53: ) -> Result<u64> {
54:     info!("Scheduling {:?} for {} (mode={:?})", kind, unit_name, mode);
55: 
56:     // --- Flush mode: cancel all pending jobs first ---
57:     if mode == JobMode::Flush {
58:         let to_cancel: Vec<u64> = {
59:             let state = allocator.read();
60:             state
61:                 .jobs
62:                 .values()
63:                 .filter(|j| matches!(j.status, JobStatus::Waiting | JobStatus::Running))
64:                 .map(|j| j.id)
65:                 .collect()
66:         };
67:         for jid in to_cancel {
68:             let mut state = allocator.write();
69:             let name = state
70:                 .jobs
71:                 .get(&jid)
72:                 .map(|j| j.unit_name.clone())
73:                 .unwrap_or_default();
74:             if let Some(job) = state.jobs.get_mut(&jid) {
75:                 job.status = JobStatus::Cancelled;
76:             }
77:             if let Some(ref tx) = state.job_completion_tx {
78:                 let _ = tx.send(JobCompletion {
79:                     job_id: jid,
80:                     unit_name: name,
81:                     result: JobResultKind::Cancelled,
82:                 });
83:             }
84:         }
85:     }
86: 
87:     // --- Condition checks: skip start (not failure) if conditions not met ---
88:     let ignore_deps = mode == JobMode::IgnoreDependencies || mode == JobMode::IgnoreRequirements;
89:     if matches!(kind, JobKind::Start | JobKind::Restart) && !ignore_deps {
90:         let conditions_met = {
91:             let state = allocator.read();
92:             state
93:                 .units
94:                 .get(unit_name)
95:                 .map(|u| check_conditions(&u.unit))
96:                 .unwrap_or(true)
97:         };
98:         if !conditions_met {
99:             info!(
100:                 "Conditions not met for {}; skipping start (unit stays inactive)",
101:                 unit_name
102:             );
103:             let job_id = next_job_id();
104:             let mut state = allocator.write();
105:             let rt = state.runtime.entry(unit_name.to_string()).or_default();
106:             rt.active_state = ActiveState::Inactive;
107:             rt.sub_state = "dead".to_string();
108:             if let Some(ref tx) = state.job_completion_tx {
109:                 let _ = tx.send(JobCompletion {
110:                     job_id,
111:                     unit_name: unit_name.to_string(),
112:                     result: JobResultKind::Skipped,
113:                 });
114:             }
115:             emit_job_new(&mut state, job_id, unit_name, kind);
116:             return Ok(job_id);
117:         }
118:     }
119: 
120:     // --- Assert checks: fail start if asserts not met ---
121:     if matches!(kind, JobKind::Start | JobKind::Restart) && !ignore_deps {
122:         let asserts_met = {
123:             let state = allocator.read();
124:             state
125:                 .units
126:                 .get(unit_name)
127:                 .map(|u| check_asserts(&u.unit))
128:                 .unwrap_or(true)
129:         };
130:         if !asserts_met {
131:             warn!(
132:                 "Assert check failed for {}; marking unit as failed",
133:                 unit_name
134:             );
135:             let job_id = next_job_id();
136:             let mut state = allocator.write();
137:             let rt = state.runtime.entry(unit_name.to_string()).or_default();
138:             rt.active_state = ActiveState::Failed;
139:             rt.sub_state = "failed".to_string();
140:             if let Some(ref tx) = state.job_completion_tx {
141:                 let _ = tx.send(JobCompletion {
142:                     job_id,
143:                     unit_name: unit_name.to_string(),
144:                     result: JobResultKind::Dependency,
145:                 });
146:             }
147:             emit_job_new(&mut state, job_id, unit_name, kind);
148:             return Ok(job_id);
149:         }
150:     }
151: 
152:     // --- Requisite check: all Requisite= deps must already be active ---
153:     if matches!(kind, JobKind::Start | JobKind::Restart) && mode != JobMode::IgnoreRequirements {
154:         let requisite_check = {
155:             let state = allocator.read();
156:             if let Some(unit) = state.units.get(unit_name) {
157:                 let mut failed_requisites = Vec::new();
158:                 for req in &unit.unit.requisite {
159:                     let is_active = state
160:                         .runtime
161:                         .get(req.as_str())
162:                         .map(|rt| matches!(rt.active_state, ActiveState::Active))
163:                         .unwrap_or(false);
164:                     if !is_active {
165:                         failed_requisites.push(req.clone());
166:                     }
167:                 }
168:                 if failed_requisites.is_empty() {
169:                     None
170:                 } else {
171:                     Some(failed_requisites)
172:                 }
173:             } else {
174:                 None
175:             }
176:         };
177:         if let Some(failed) = requisite_check {
178:             warn!(
179:                 "Requisite check failed for {}: required units not active: {:?}",
180:                 unit_name, failed
181:             );
182:             let job_id = next_job_id();
183:             let mut state = allocator.write();
184:             let rt = state.runtime.entry(unit_name.to_string()).or_default();
185:             rt.active_state = ActiveState::Failed;
186:             rt.sub_state = "failed".to_string();
187:             if let Some(ref tx) = state.job_completion_tx {
188:                 let _ = tx.send(JobCompletion {
189:                     job_id,
190:                     unit_name: unit_name.to_string(),
191:                     result: JobResultKind::Dependency,
192:                 });
193:             }
194:             emit_job_new(&mut state, job_id, unit_name, kind);
195:             return Ok(job_id);
196:         }
197:     }
198: 
199:     // --- Conflicts handling: stop conflicting units when starting ---
200:     if matches!(kind, JobKind::Start | JobKind::Restart) {
201:         let conflicts: Vec<String> = {
202:             let state = allocator.read();
203:             state
204:                 .units
205:                 .get(unit_name)
206:                 .map(|u| u.unit.conflicts.iter().cloned().collect())
207:                 .unwrap_or_default()
208:         };
209:         for conflict in conflicts {
210:             let is_active = {
211:                 let state = allocator.read();
212:                 state
213:                     .runtime
214:                     .get(conflict.as_str())
215:                     .map(|rt| {
216:                         matches!(
217:                             rt.active_state,
218:                             ActiveState::Active | ActiveState::Activating
219:                         )
220:                     })
221:                     .unwrap_or(false)
222:             };
223:             if is_active {
224:                 info!(
225:                     "Stopping conflicting unit {} before starting {}",
226:                     conflict, unit_name
227:                 );
228:                 Box::pin(enqueue_job(allocator.clone(), &conflict, JobKind::Stop, JobMode::Replace)).await?;
229:             }
230:         }
231:     }
232: 
233:     // --- Job conflict detection ---
234:     {
235:         let read_state = allocator.read();
236:         let existing: Option<(u64, String)> = read_state
237:             .jobs
238:             .values()
239:             .find(|j| {
240:                 j.unit_name == unit_name
241:                     && j.kind == kind
242:                     && matches!(j.status, JobStatus::Running | JobStatus::Waiting)
243:             })
244:             .map(|j| (j.id, j.unit_name.clone()));
245:         drop(read_state);
246: 
247:         if let Some((existing_id, _)) = existing {
248:             match mode {
249:                 JobMode::Fail => {
250:                     bail!(
251:                         "Job already exists for unit {} (kind={:?}, id={})",
252:                         unit_name,
253:                         kind,
254:                         existing_id
255:                     );
256:                 }
257:                 JobMode::Queue => {
258:                     return Ok(existing_id);
259:                 }
260:                 JobMode::Replace => {
261:                     let mut wstate = allocator.write();
262:                     if let Some(job) = wstate.jobs.get_mut(&existing_id) {
263:                         job.status = JobStatus::Cancelled;
264:                     }
265:                 }
266:                 _ => {}
267:             }
268:         }
269:     }
270: 
271:     // Determine which units to activate in dependency order.
272:     let units_to_process = {
273:         let state = allocator.read();
274:         match kind {
275:             JobKind::Start | JobKind::Restart
276:                 if mode == JobMode::IgnoreDependencies || mode == JobMode::IgnoreRequirements =>
277:             {
278:                 vec![unit_name.to_string()]
279:             }
280:             JobKind::Start | JobKind::Restart => compute_start_order(&state.units, unit_name),
281:             JobKind::Stop => {
282:                 compute_stop_order(&state.units, &state.runtime, unit_name)
283:             }
284:             JobKind::Reload => {
285:                 compute_reload_order(&state.units, unit_name)
286:             }
287:         }
288:     };
289: 
290:     // --- Isolate mode: stop all running units not in the dependency tree ---
291:     if mode == JobMode::Isolate && matches!(kind, JobKind::Start) {
292:         handle_isolate(allocator.clone(), &units_to_process).await;
293:     }
294: 
295:     debug!("Processing order: {:?}", units_to_process);
296: 
297:     let primary_job_id = next_job_id();
298:     let serial_mode = mode == JobMode::Replace;
299: 
300:     // For serial execution, chain tasks so each waits for the previous.
301:     let mut serial_chain_rx: Option<tokio::sync::oneshot::Receiver<()>> = None;
302: 
303:     // Create job records and dispatch tasks.
304:     for name in units_to_process.iter() {
305:         // The primary job ID belongs to the unit that was directly requested.
306:         let is_root = name.as_str() == unit_name;
307:         let job_id = if is_root {
308:             primary_job_id
309:         } else {
310:             next_job_id()
311:         };
312: 
313:         // Idempotency: skip if the unit is already in the desired state,
314:         // or if there is already an in-flight job of the same kind for it.
315:         {
316:             let state = allocator.read();
317:             let already_in_desired_state = match kind {
318:                 JobKind::Start => state
319:                     .runtime
320:                     .get(name.as_str())
321:                     .map(|rt| {
322:                         matches!(
323:                             rt.active_state,
324:                             ActiveState::Active | ActiveState::Activating
325:                         )
326:                     })
327:                     .unwrap_or(false),
328:                 // Restart always re-executes: stop then start, regardless of current state.
329:                 JobKind::Restart => false,
330:                 JobKind::Stop => {
331:                     state
332:                         .runtime
333:                         .get(name.as_str())
334:                         .map(|rt| {
335:                             matches!(
336:                                 rt.active_state,
337:                                 ActiveState::Inactive | ActiveState::Deactivating
338:                             )
339:                         })
340:                         .unwrap_or(true) // treat unknown as inactive for stop
341:                 }
342:                 JobKind::Reload => false,
343:             };
344: 
345:             // Check for an existing running/waiting job of the same kind.
346:             let existing_running_job_id: Option<u64> = state
347:                 .jobs
348:                 .values()
349:                 .find(|j| {
350:                     j.unit_name == *name
351:                         && j.kind == kind
352:                         && matches!(j.status, JobStatus::Running | JobStatus::Waiting)
353:                 })
354:                 .map(|j| j.id);
355: 
356:             if already_in_desired_state {
357:                 let reason = "already in desired state";
358:                 debug!("Skipping {:?} for {} ({})", kind, name, reason);
359:                 if is_root {
360:                     // Unit is already in the desired state — send an immediate
361:                     // completion so the caller (e.g. systemctl) doesn't wait.
362:                     if let Some(ref tx) = state.job_completion_tx {
363:                         let _ = tx.send(JobCompletion {
364:                             job_id: primary_job_id,
365:                             unit_name: unit_name.to_string(),
366:                             result: JobResultKind::Done,
367:                         });
368:                     }
369:                 }
370:                 continue;
371:             }
372: 
373:             if let Some(existing_jid) = existing_running_job_id {
374:                 debug!("Skipping {:?} for {} (existing job running)", kind, name);
375:                 if is_root {
376:                     // There is already an in-flight job for this operation.
377:                     // Return its ID so the caller waits for the REAL completion
378:                     // signal rather than a phantom new ID that will never fire.
379:                     return Ok(existing_jid);
380:                 }
381:                 continue;
382:             }
383:         }
384: 
385:         // Target units are handled internally (no external worker needed).
386:         let is_target = {
387:             let state = allocator.read();
388:             state
389:                 .units
390:                 .get(name.as_str())
391:                 .map_or(false, |u| matches!(u.kind, UnitKind::Target))
392:         };
393:         if is_target {
394:             activate_target_internally(allocator.clone(), name);
395:             emit_job_new_after_lock(allocator.clone(), job_id, name, kind);
396:             if is_root {
397:                 if let Some(ref tx) = allocator.read().job_completion_tx {
398:                     let _ = tx.send(JobCompletion {
399:                         job_id: primary_job_id,
400:                         unit_name: unit_name.to_string(),
401:                         result: JobResultKind::Done,
402:                     });
403:                 }
404:             }
405:             continue;
406:         }
407: 
408:         // Find the appropriate worker.
409:         let (worker_task_tx, task_id, unit_type) = {
410:             let state = allocator.read();
411:             let unit = state.units.get(name.as_str());
412:             let unit_type = unit
413:                 .map(|u| u.kind.worker_type().to_string())
414:                 .unwrap_or_else(|| "service".to_string());
415: 
416:             let worker = state
417:                 .workers
418:                 .values()
419:                 .find(|w| w.unit_types.contains(&unit_type));
420: 
421:             match worker {
422:                 Some(w) => {
423:                     let tx = w.task_tx.clone();
424:                     let tid = next_task_id();
425:                     (tx, tid, unit_type)
426:                 }
427:                 None => {
428:                     warn!(
429:                         "No worker registered for unit type '{}' (unit: {})",
430:                         unit_type, name
431:                     );
432:                     if is_root {
433:                         let mut state = allocator.write();
434:                         let rt = state.runtime.entry(name.clone()).or_default();
435:                         rt.active_state = ActiveState::Failed;
436:                         rt.sub_state = "failed".to_string();
437:                         emit_job_new(&mut state, job_id, name, kind);
438:                         if let Some(ref tx) = state.job_completion_tx {
439:                             let _ = tx.send(JobCompletion {
440:                                 job_id: primary_job_id,
441:                                 unit_name: name.clone(),
442:                                 result: JobResultKind::Failed,
443:                             });
444:                         }
445:                     }
446:                     continue;
447:                 }
448:             }
449:         };
450: 
451:         let unit_file = {
452:             let state = allocator.read();
453:             state.units.get(name.as_str()).cloned()
454:         };
455: 
456:         // Create a serial chain entry for this task.
457:         let (next_serial_tx, next_serial_rx) = tokio::sync::oneshot::channel::<()>();
458: 
459:         // Record the job and the task_id → job_kind mapping.
460:         {
461:             let mut state = allocator.write();
462:             let rt = state.runtime.entry(name.clone()).or_default();
463:             rt.load_state = "loaded".to_string();
464:             match kind {
465:                 JobKind::Start | JobKind::Restart => {
466:                     rt.active_state = ActiveState::Activating;
467:                     rt.sub_state = "start".to_string();
468:                 }
469:                 JobKind::Stop => {
470:                     rt.active_state = ActiveState::Deactivating;
471:                     rt.sub_state = "stop".to_string();
472:                 }
473:                 JobKind::Reload => {}
474:             }
475:             state.jobs.insert(
476:                 job_id,
477:                 Job {
478:                     id: job_id,
479:                     unit_name: name.clone(),
480:                     kind,
481:                     status: JobStatus::Running,
482:                     completion_tx: None,
483:                 },
484:             );
485:             state.task_kinds.insert(task_id, kind);
486:             if serial_mode {
487:                 state.serial_completion_txs.insert(task_id, next_serial_tx);
488:             }
489:         }
490: 
491:         // Emit JobNew signal for this job.
492:         emit_job_new_after_lock(allocator.clone(), job_id, name, kind);
493: 
494:         // --- Start timeout monitoring ---
495:         if matches!(kind, JobKind::Start | JobKind::Restart) {
496:             let timeout_secs = unit_file
497:                 .as_ref()
498:                 .and_then(|u| u.service.as_ref())
499:                 .map(|s| s.timeout_start_sec)
500:                 .filter(|&t| t > 0)
501:                 .unwrap_or(0);
502:             if timeout_secs > 0 {
503:                 let alloc = allocator.clone();
504:                 let name_clone = name.clone();
505:                 tokio::spawn(async move {
506:                     tokio::time::sleep(Duration::from_secs(timeout_secs as u64)).await;
507:                     let mut state = alloc.write();
508:                     if let Some(job) = state.jobs.get_mut(&job_id) {
509:                         if matches!(job.status, JobStatus::Running) {
510:                             job.status = JobStatus::Failed("TimeoutStartSec exceeded".to_string());
511:                             let rt = state.runtime.entry(name_clone.clone()).or_default();
512:                             rt.active_state = ActiveState::Failed;
513:                             rt.sub_state = "failed".to_string();
514:                             if let Some(ref tx) = state.job_completion_tx {
515:                                 let _ = tx.send(JobCompletion {
516:                                     job_id,
517:                                     unit_name: name_clone,
518:                                     result: JobResultKind::Timeout,
519:                                 });
520:                             }
521:                         }
522:                     }
523:                 });
524:             }
525:         }
526: 
527:         // --- Job running timeout monitoring ---
528:         if matches!(kind, JobKind::Start) {
529:             let timeout_secs = unit_file
530:                 .as_ref()
531:                 .and_then(|u| u.service.as_ref())
532:                 .map(|s| s.timeout_start_sec)
533:                 .filter(|&t| t > 0)
534:                 .unwrap_or(90);
535:             let alloc = allocator.clone();
536:             let name_clone = name.clone();
537:             tokio::spawn(async move {
538:                 tokio::time::sleep(Duration::from_secs(timeout_secs as u64 * 2)).await;
539:                 let mut state = alloc.write();
540:                 if let Some(job) = state.jobs.get_mut(&job_id) {
541:                     if matches!(job.status, JobStatus::Running) {
542:                         job.status = JobStatus::Failed("JobRunningTimeout exceeded".to_string());
543:                         let rt = state.runtime.entry(name_clone.clone()).or_default();
544:                         rt.active_state = ActiveState::Failed;
545:                         rt.sub_state = "failed".to_string();
546:                         if let Some(ref tx) = state.job_completion_tx {
547:                             let _ = tx.send(JobCompletion {
548:                                 job_id,
549:                                 unit_name: name_clone,
550:                                 result: JobResultKind::Timeout,
551:                             });
552:                         }
553:                     }
554:                 }
555:             });
556:         }
557: 
558:         let task = WorkerTask {
559:             task_id,
560:             unit_name: name.clone(),
561:             unit_type,
562:             kind,
563:             unit_file,
564:         };
565: 
566:         // In serial mode, wait for the previous task to complete before
567:         // sending the next one.  The previous task's handle_task_result
568:         // will signal through the serial chain channel.
569:         if serial_mode && is_root && serial_chain_rx.is_some() {
570:             if let Some(rx) = serial_chain_rx.take() {
571:                 let _ = rx.await;
572:             }
573:         }
574:         if serial_mode && !is_root {
575:             if let Some(rx) = serial_chain_rx.take() {
576:                 let _ = rx.await;
577:             }
578:         }
579: 
580:         if worker_task_tx.send(task).await.is_err() {
581:             warn!("Worker channel closed for unit {}", name);
582:             let mut state = allocator.write();
583:             if let Some(job) = state.jobs.get_mut(&job_id) {
584:                 job.status = JobStatus::Failed("Worker disconnected".to_string());
585:             }
586:             let rt = state.runtime.entry(name.clone()).or_default();
587:             rt.active_state = ActiveState::Failed;
588:             rt.sub_state = "failed".to_string();
589:             if is_root {
590:                 if let Some(ref tx) = state.job_completion_tx {
591:                     let _ = tx.send(JobCompletion {
592:                         job_id,
593:                         unit_name: name.clone(),
594:                         result: JobResultKind::Failed,
595:                     });
596:                 }
597:             }
598:         }
599: 
600:         // Set up for the next iteration: the current task's serial_rx
601:         // will be consumed after the next task completes.
602:         serial_chain_rx = Some(next_serial_rx);
603:     }
604: 
605:     Ok(primary_job_id)
606: }
607: 
608: /// Handle isolate mode: stop all running units not in the dependency tree.
609: async fn handle_isolate(allocator: AllocatorHandle, keep_units: &[String]) {
610:     let to_stop: Vec<String> = {
611:         let state = allocator.read();
612:         state
613:             .runtime
614:             .iter()
615:             .filter(|(name, rt)| {
616:                 matches!(rt.active_state, ActiveState::Active | ActiveState::Activating)
617:                     && !keep_units.contains(name)
618:             })
619:             .map(|(name, _)| name.clone())
620:             .collect()
621:     };
622:     for name in to_stop {
623:         info!("Isolate mode: stopping unit {}", name);
624:         if let Err(e) = Box::pin(enqueue_job(allocator.clone(), &name, JobKind::Stop, JobMode::Replace)).await
625:         {
626:             warn!("Failed to stop {} during isolate: {}", name, e);
627:         }
628:     }
629: }
630: 
631: /// Emit a JobNew signal for a newly created job (must hold the write lock).
632: fn emit_job_new(state: &mut AllocatorState, job_id: u64, unit_name: &str, kind: JobKind) {
633:     if let Some(ref tx) = state.job_new_tx {
634:         let _ = tx.send(JobNewInfo {
635:             job_id,
636:             unit_name: unit_name.to_string(),
637:             kind,
638:         });
639:     }
640: }
641: 
642: /// Emit JobNew without holding the write lock (acquires it briefly).
643: fn emit_job_new_after_lock(allocator: AllocatorHandle, job_id: u64, unit_name: &str, kind: JobKind) {
644:     let state = allocator.read();
645:     if let Some(ref tx) = state.job_new_tx {
646:         let _ = tx.send(JobNewInfo {
647:             job_id,
648:             unit_name: unit_name.to_string(),
649:             kind,
650:         });
651:     }
652: }
653: 
654: /// Compute the order in which units should be stopped.
655: /// This includes the requested unit plus all units that have a hard dependency
656: /// on it (Requires=, BindsTo=, PartOf=).
657: fn compute_stop_order(
658:     units: &std::collections::HashMap<String, UnitFile>,
659:     runtime: &std::collections::HashMap<String, UnitRuntimeInfo>,
660:     root: &str,
661: ) -> Vec<String> {
662:     use std::collections::HashSet;
663: 
664:     let mut result = Vec::new();
665:     let mut visited = HashSet::new();
666:     let mut stack = vec![root.to_string()];
667: 
668:     while let Some(name) = stack.pop() {
669:         if visited.contains(&name) {
670:             continue;
671:         }
672:         visited.insert(name.clone());
673:         result.push(name.clone());
674: 
675:         // Find all units that have Requires=name, BindsTo=name, or PartOf=name
676:         // and are currently active — they must be stopped too.
677:         for (other_name, other_unit) in units {
678:             if visited.contains(other_name) {
679:                 continue;
680:             }
681:             let depends_on_name = other_unit.unit.requires.contains(&name)
682:                 || other_unit.unit.binds_to.contains(&name)
683:                 || other_unit.unit.part_of.contains(&name);
684:             if depends_on_name {
685:                 let is_active = runtime
686:                     .get(other_name.as_str())
687:                     .map(|rt| {
688:                         matches!(
689:                             rt.active_state,
690:                             ActiveState::Active | ActiveState::Activating
691:                         )
692:                     })
693:                     .unwrap_or(false);
694:                 if is_active {
695:                     stack.push(other_name.clone());
696:                 }
697:             }
698:         }
699:     }
700: 
701:     result
702: }
703: 
704: /// Compute the reload propagation order.
705: /// Includes the root unit plus all units listed in its PropagatesReloadTo=.
706: fn compute_reload_order(
707:     units: &std::collections::HashMap<String, UnitFile>,
708:     root: &str,
709: ) -> Vec<String> {
710:     let mut result = vec![root.to_string()];
711:     if let Some(unit) = units.get(root) {
712:         for target in &unit.unit.propagates_reload_to {
713:             if !result.contains(target) {
714:                 result.push(target.clone());
715:             }
716:         }
717:     }
718:     result
719: }
720: 
721: /// Compute the order in which units should be started, respecting After/Before.
722: fn compute_start_order(
723:     units: &std::collections::HashMap<String, UnitFile>,
724:     root: &str,
725: ) -> Vec<String> {
726:     use std::collections::{HashSet, VecDeque};
727: 
728:     let mut result = Vec::new();
729:     let mut visited = HashSet::new();
730:     let mut queue = VecDeque::new();
731: 
732:     // Collect all units reachable via Requires/Wants/Requisite/BindsTo/Upholds from root.
733:     queue.push_back(root.to_string());
734:     let mut reachable = HashSet::new();
735:     while let Some(name) = queue.pop_front() {
736:         if reachable.contains(&name) {
737:             continue;
738:         }
739:         reachable.insert(name.clone());
740:         if let Some(unit) = units.get(&name) {
741:             for dep in unit
742:                 .unit
743:                 .requires
744:                 .iter()
745:                 .chain(unit.unit.wants.iter())
746:                 .chain(unit.unit.requisite.iter())
747:                 .chain(unit.unit.binds_to.iter())
748:             {
749:                 queue.push_back(dep.clone());
750:             }
751:         }
752:     }
753: 
754:     // Simple DFS-based topological sort respecting After ordering.
755:     fn visit(
756:         name: &str,
757:         units: &std::collections::HashMap<String, UnitFile>,
758:         visited: &mut HashSet<String>,
759:         result: &mut Vec<String>,
760:     ) {
761:         if visited.contains(name) {
762:             return;
763:         }
764:         visited.insert(name.to_string());
765:         // Visit After-dependencies first.
766:         if let Some(unit) = units.get(name) {
767:             let afters: Vec<String> = unit.unit.after.iter().cloned().collect();
768:             for dep in afters {
769:                 visit(&dep, units, visited, result);
770:             }
771:         }
772:         result.push(name.to_string());
773:     }
774: 
775:     for name in &reachable {
776:         visit(name, units, &mut visited, &mut result);
777:     }
778: 
779:     // Ensure the root unit appears last.
780:     if let Some(pos) = result.iter().position(|n| n == root) {
781:         let last = result.len() - 1;
782:         result.swap(pos, last);
783:     }
784: 
785:     result
786: }
787: 
788: /// Activate a target unit inline in System A (no external worker needed).
789: fn activate_target_internally(allocator: AllocatorHandle, name: &str) {
790:     let mut state = allocator.write();
791:     let rt = state.runtime.entry(name.to_string()).or_default();
792:     rt.active_state = ActiveState::Active;
793:     rt.sub_state = "active".to_string();
794:     rt.load_state = "loaded".to_string();
795:     debug!("Target {} activated internally", name);
796: }
797: 
798: /// Update unit runtime state from a task result received from a worker.
799: pub fn handle_task_result(
800:     allocator: AllocatorHandle,
801:     task_id: u64,
802:     success: bool,
803:     message: &str,
804:     unit_name: &str,
805:     kind: JobKind,
806: ) {
807:     let mut post_actions: Vec<PostAction> = Vec::new();
808:     let mut should_restart = false;
809:     let mut restart_delay = 0u64;
810: 
811:     {
812:         let mut state = allocator.write();
813: 
814:         // Clean up the task_id → kind mapping.
815:         state.task_kinds.remove(&task_id);
816: 
817:         // Signal serial chain continuation if this task was part of one.
818:         if let Some(tx) = state.serial_completion_txs.remove(&task_id) {
819:             let _ = tx.send(());
820:         }
821: 
822:         // Update active state based on task kind and success.
823:         let rt = state.runtime.entry(unit_name.to_string()).or_default();
824:         if success {
825:             rt.active_state = match kind {
826:                 JobKind::Start | JobKind::Restart => ActiveState::Active,
827:                 JobKind::Stop => ActiveState::Inactive,
828:                 JobKind::Reload => ActiveState::Active,
829:             };
830:             rt.sub_state = if kind == JobKind::Stop {
831:                 "dead".to_string()
832:             } else {
833:                 "running".to_string()
834:             };
835:         } else {
836:             rt.active_state = ActiveState::Failed;
837:             rt.sub_state = "failed".to_string();
838:         }
839: 
840:         // Find and update the associated job.
841:         let job_id = state
842:             .jobs
843:             .values()
844:             .find(|j| j.unit_name == unit_name && matches!(j.status, JobStatus::Running))
845:             .map(|j| j.id);
846: 
847:         if let Some(jid) = job_id {
848:             let result_kind = if success {
849:                 JobResultKind::Done
850:             } else {
851:                 JobResultKind::Failed
852:             };
853:             let result = JobResult {
854:                 job_id: jid,
855:                 unit_name: unit_name.to_string(),
856:                 result: result_kind.clone(),
857:             };
858:             if let Some(job) = state.jobs.get_mut(&jid) {
859:                 job.status = if success {
860:                     JobStatus::Done
861:                 } else {
862:                     JobStatus::Failed(message.to_string())
863:                 };
864:                 if let Some(tx) = job.completion_tx.take() {
865:                     let _ = tx.send(result);
866:                 }
867:             }
868:             if let Some(ref tx) = state.job_completion_tx {
869:                 let _ = tx.send(JobCompletion {
870:                     job_id: jid,
871:                     unit_name: unit_name.to_string(),
872:                     result: result_kind,
873:                 });
874:             }
875:         }
876: 
877:         // --- BindsTo= lifecycle binding ---
878:         if matches!(kind, JobKind::Stop) || !success {
879:             for (other_name, other_unit) in &state.units {
880:                 if other_unit.unit.binds_to.contains(unit_name) {
881:                     let is_active = state
882:                         .runtime
883:                         .get(other_name.as_str())
884:                         .map(|rt| {
885:                             matches!(
886:                                 rt.active_state,
887:                                 ActiveState::Active | ActiveState::Activating
888:                             )
889:                         })
890:                         .unwrap_or(false);
891:                     if is_active {
892:                         post_actions.push(PostAction::Stop(other_name.clone()));
893:                     }
894:                 }
895:             }
896:         }
897: 
898:         // --- PartOf= stop propagation ---
899:         if matches!(kind, JobKind::Stop) {
900:             for (other_name, other_unit) in &state.units {
901:                 if other_unit.unit.part_of.contains(unit_name) {
902:                     let is_active = state
903:                         .runtime
904:                         .get(other_name.as_str())
905:                         .map(|rt| {
906:                             matches!(
907:                                 rt.active_state,
908:                                 ActiveState::Active | ActiveState::Activating
909:                             )
910:                         })
911:                         .unwrap_or(false);
912:                     if is_active {
913:                         post_actions.push(PostAction::Stop(other_name.clone()));
914:                     }
915:                 }
916:             }
917:         }
918: 
919:         // --- OnSuccess= / OnFailure= triggers ---
920:         if let Some(unit) = state.units.get(unit_name) {
921:             if success && matches!(kind, JobKind::Start | JobKind::Restart) {
922:                 for target in &unit.unit.on_success {
923:                     post_actions.push(PostAction::Start(target.clone()));
924:                 }
925:             }
926:             if !success {
927:                 for target in &unit.unit.on_failure {
928:                     post_actions.push(PostAction::Start(target.clone()));
929:                 }
930:             }
931:         }
932: 
933:         // --- Upholds= continuous activation ---
934:         if (!success || matches!(kind, JobKind::Stop))
935:             && matches!(
936:                 state.runtime.get(unit_name).map(|rt| &rt.active_state),
937:                 Some(ActiveState::Inactive) | Some(ActiveState::Failed)
938:             )
939:         {
940:             for (_other_name, other_unit) in &state.units {
941:                 if other_unit.unit.upholds.contains(unit_name) {
942:                     let upholder_active = state
943:                         .runtime
944:                         .get(_other_name.as_str())
945:                         .map(|rt| matches!(rt.active_state, ActiveState::Active))
946:                         .unwrap_or(false);
947:                     if upholder_active {
948:                         post_actions.push(PostAction::Start(unit_name.to_string()));
949:                         break;
950:                     }
951:                 }
952:             }
953:         }
954: 
955:         // --- Restart policy: check if the failed unit should be restarted ---
956:         // Extract info under the lock, then process after releasing it.
957:         let restart_policy: Option<(RestartPolicy, u32)> = state
958:             .units
959:             .get(unit_name)
960:             .and_then(|u| u.service.as_ref())
961:             .map(|svc| (svc.restart.clone(), svc.restart_sec));
962:         if let Some((ref policy, restart_sec_val)) = restart_policy {
963:             if !success && matches!(kind, JobKind::Start) {
964:                 let should = match policy {
965:                     RestartPolicy::Always => true,
966:                     RestartPolicy::OnFailure => {
967:                         !matches!(
968:                             message,
969:                             "TimeoutStartSec exceeded" | "JobRunningTimeout exceeded"
970:                         )
971:                     }
972:                     RestartPolicy::OnAbnormal => {
973:                         !matches!(message, "exit code")
974:                     }
975:                     _ => false,
976:                 };
977:                 if should {
978:                     // Check rate limit (needs lock, will re-acquire)
979:                     let interval = Duration::from_secs(10);
980:                     let burst = 5u32;
981:                     drop(state);
982:                     let mut wstate = allocator.write();
983:                     let limit_state = wstate
984:                         .start_limit_state
985:                         .entry(unit_name.to_string())
986:                         .or_insert_with(StartLimitState::new);
987:                     if limit_state.check_rate_limit(interval, burst) {
988:                         should_restart = true;
989:                         restart_delay = restart_sec_val as u64;
990:                     } else {
991:                         warn!(
992:                             "Restart rate limit exceeded for {}, not restarting",
993:                             unit_name
994:                         );
995:                     }
996:                     // Note: wstate goes out of scope here and the write lock is released
997:                 }
998:             }
999:         }
1000:     } // drop write lock
1001: 
1002:     // Execute post-actions asynchronously.
1003:     if !post_actions.is_empty() {
1004:         let alloc = allocator.clone();
1005:         tokio::spawn(async move {
1006:             for action in post_actions {
1007:                 match action {
1008:                     PostAction::Stop(name) => {
1009:                         info!("Propagating stop to {}", name);
1010:                         if let Err(e) =
1011:                             enqueue_job(alloc.clone(), &name, JobKind::Stop, JobMode::Replace).await
1012:                         {
1013:                             warn!("Failed to propagate stop to {}: {}", name, e);
1014:                         }
1015:                     }
1016:                     PostAction::Start(name) => {
1017:                         info!("Triggering start for {}", name);
1018:                         if let Err(e) =
1019:                             enqueue_job(alloc.clone(), &name, JobKind::Start, JobMode::Replace).await
1020:                         {
1021:                             warn!("Failed to trigger start for {}: {}", name, e);
1022:                         }
1023:                     }
1024:                 }
1025:             }
1026:         });
1027:     }
1028: 
1029:     // Schedule restart if the restart policy triggered.
1030:     if should_restart {
1031:         let alloc = allocator.clone();
1032:         let name = unit_name.to_string();
1033:         tokio::spawn(async move {
1034:             if restart_delay > 0 {
1035:                 tokio::time::sleep(Duration::from_secs(restart_delay)).await;
1036:             }
1037:             info!("Restarting {} per restart policy", name);
1038:             if let Err(e) =
1039:                 enqueue_job(alloc.clone(), &name, JobKind::Start, JobMode::Replace).await
1040:             {
1041:                 warn!("Failed to restart {}: {}", name, e);
1042:             }
1043:         });
1044:     }
1045: }
1046: 
1047: /// Post-processing actions to be taken after a task result is handled.
1048: enum PostAction {
1049:     Stop(String),
1050:     Start(String),
1051: }
1052: 
1053: /// Build a `TaskDispatch` protobuf from a `WorkerTask`.
1054: pub fn build_task_dispatch(task: &WorkerTask) -> TaskDispatch {
1055:     let unit_config = task.unit_file.as_ref().map(|uf| build_unit_config(uf));
1056:     let unit_config_bytes = unit_config
1057:         .map(|c| {
1058:             let mut buf = bytes::BytesMut::new();
1059:             c.encode(&mut buf).unwrap_or_default();
1060:             buf.to_vec()
1061:         })
1062:         .unwrap_or_default();
1063: 
1064:     TaskDispatch {
1065:         task_id: task.task_id,
1066:         unit_name: task.unit_name.clone(),
1067:         unit_type: task.unit_type.clone(),
1068:         kind: task_kind_to_proto(task.kind) as i32,
1069:         unit_config: unit_config_bytes,
1070:     }
1071: }
1072: 
1073: fn task_kind_to_proto(kind: JobKind) -> TaskKind {
1074:     match kind {
1075:         JobKind::Start => TaskKind::Start,
1076:         JobKind::Stop => TaskKind::Stop,
1077:         JobKind::Restart => TaskKind::Restart,
1078:         JobKind::Reload => TaskKind::Reload,
1079:     }
1080: }
1081: 
1082: fn build_unit_config(uf: &UnitFile) -> UnitConfig {
1083:     let service = uf.service.as_ref().map(|svc| ServiceConfig {
1084:         // Only the first ExecStart command is sent to the worker.
1085:         // Multiple ExecStart directives (Type=oneshot) will be supported in Phase 2.
1086:         exec_start: svc.exec_start.first().map(|c| c.raw.clone()).unwrap_or_default(),
1087:         exec_stop: svc.exec_stop.first().map(|c| c.raw.clone()).unwrap_or_default(),
1088:         exec_reload: svc.exec_reload.first().map(|c| c.raw.clone()).unwrap_or_default(),
1089:         working_directory: svc.working_directory.clone(),
1090:         user: svc.user.clone(),
1091:         group: svc.group.clone(),
1092:         environment: svc.environment.clone(),
1093:         restart_policy: svc.restart.as_str().to_string(),
1094:         restart_delay_secs: svc.restart_sec,
1095:         service_type: svc.service_type.as_str().to_string(),
1096:         pid_file: svc.pid_file.clone(),
1097:         timeout_start_secs: svc.timeout_start_sec,
1098:         timeout_stop_secs: svc.timeout_stop_sec,
1099:     });
1100: 
1101:     UnitConfig {
1102:         unit_name: uf.name.clone(),
1103:         description: uf.unit.description.clone(),
1104:         service,
1105:     }
1106: }
1107: 
1108: // ---------------------------------------------------------------------------
1109: // Condition and assert evaluation
1110: // ---------------------------------------------------------------------------
1111: 
1112: /// Evaluate all `Condition*=` directives in `unit`.
1113: ///
1114: /// Returns `true` if all conditions pass (unit should start), `false` if any
1115: /// condition fails (unit should be silently skipped, staying inactive).
1116: ///
1117: /// A value prefixed with `!` negates the check.
1118: fn check_conditions(unit: &UnitSection) -> bool {
1119:     for path in &unit.condition_path_exists {
1120:         if !eval_condition_bool(path, |p| std::path::Path::new(p).exists()) {
1121:             return false;
1122:         }
1123:     }
1124:     for glob in &unit.condition_path_exists_glob {
1125:         if !eval_condition_bool(glob, |g| path_glob_matches(g)) {
1126:             return false;
1127:         }
1128:     }
1129:     for path in &unit.condition_file_not_empty {
1130:         if !eval_condition_bool(path, |p| {
1131:             std::fs::metadata(p)
1132:                 .map(|m| m.len() != 0)
1133:                 .unwrap_or(false)
1134:         }) {
1135:             return false;
1136:         }
1137:     }
1138:     for path in &unit.condition_directory_not_empty {
1139:         if !eval_condition_bool(path, |p| {
1140:             std::fs::read_dir(p)
1141:                 .map(|mut d| d.next().is_some())
1142:                 .unwrap_or(false)
1143:         }) {
1144:             return false;
1145:         }
1146:     }
1147:     for spec in &unit.condition_ac_power {
1148:         let (negate, value) = strip_negate(spec);
1149:         let on_ac = is_on_ac_power();
1150:         let want = matches!(value.to_lowercase().as_str(), "yes" | "true" | "1");
1151:         if (on_ac != want) != negate {
1152:             return false;
1153:         }
1154:     }
1155:     // ConditionFirstBoot=yes passes only on the first boot.
1156:     for spec in &unit.condition_first_boot {
1157:         let (negate, value) = strip_negate(spec);
1158:         let first = is_first_boot();
1159:         let want = matches!(value.to_lowercase().as_str(), "yes" | "true" | "1");
1160:         if (first != want) != negate {
1161:             return false;
1162:         }
1163:     }
1164:     true
1165: }
1166: 
1167: /// Evaluate all `Assert*=` directives in `unit`.
1168: ///
1169: /// Returns `true` if all asserts pass, `false` if any assert fails (unit
1170: /// should be marked failed, not just skipped).
1171: fn check_asserts(unit: &UnitSection) -> bool {
1172:     for path in &unit.assert_path_exists {
1173:         if !eval_condition_bool(path, |p| std::path::Path::new(p).exists()) {
1174:             return false;
1175:         }
1176:     }
1177:     for glob in &unit.assert_path_exists_glob {
1178:         if !eval_condition_bool(glob, |g| path_glob_matches(g)) {
1179:             return false;
1180:         }
1181:     }
1182:     for path in &unit.assert_file_not_empty {
1183:         if !eval_condition_bool(path, |p| {
1184:             std::fs::metadata(p)
1185:                 .map(|m| m.len() != 0)
1186:                 .unwrap_or(false)
1187:         }) {
1188:             return false;
1189:         }
1190:     }
1191:     for path in &unit.assert_directory_not_empty {
1192:         if !eval_condition_bool(path, |p| {
1193:             std::fs::read_dir(p)
1194:                 .map(|mut d| d.next().is_some())
1195:                 .unwrap_or(false)
1196:         }) {
1197:             return false;
1198:         }
1199:     }
1200:     for spec in &unit.assert_first_boot {
1201:         let (negate, value) = strip_negate(spec);
1202:         let first = is_first_boot();
1203:         let want = matches!(value.to_lowercase().as_str(), "yes" | "true" | "1");
1204:         if (first != want) != negate {
1205:             return false;
1206:         }
1207:     }
1208:     true
1209: }
1210: 
1211: /// Strip a leading `!` from `spec`, returning `(negated, rest)`.
1212: fn strip_negate(spec: &str) -> (bool, &str) {
1213:     if let Some(rest) = spec.strip_prefix('!') {
1214:         (true, rest)
1215:     } else {
1216:         (false, spec)
1217:     }
1218: }
1219: 
1220: /// Evaluate a single condition string against a predicate.
1221: ///
1222: /// If the spec starts with `!`, the result is negated.
1223: fn eval_condition_bool<F: Fn(&str) -> bool>(spec: &str, pred: F) -> bool {
1224:     let (negate, path) = strip_negate(spec);
1225:     let result = pred(path);
1226:     if negate { !result } else { result }
1227: }
1228: 
1229: /// Check if any filesystem path matches a simple glob pattern.
1230: ///
1231: /// Uses the same glob logic as the rest of systema (no external crate).
1232: fn path_glob_matches(pattern: &str) -> bool {
1233:     // Split into directory and file-name glob parts.
1234:     let (dir, file_pattern) = match pattern.rfind('/') {
1235:         Some(pos) => (&pattern[..pos], &pattern[pos + 1..]),
1236:         None => (".", pattern),
1237:     };
1238:     std::fs::read_dir(dir)
1239:         .map(|entries| {
1240:             entries.filter_map(|e| e.ok()).any(|e| {
1241:                 e.file_name()
1242:                     .to_str()
1243:                     .map(|n| simple_glob_match(file_pattern, n))
1244:                     .unwrap_or(false)
1245:             })
1246:         })
1247:         .unwrap_or(false)
1248: }
1249: 
1250: /// Minimal shell-style glob: `*` matches any sequence, `?` matches any char.
1251: fn simple_glob_match(pattern: &str, text: &str) -> bool {
1252:     let pat: Vec<char> = pattern.chars().collect();
1253:     let txt: Vec<char> = text.chars().collect();
1254:     glob_match_impl(&pat, &txt)
1255: }
1256: 
1257: fn glob_match_impl(pat: &[char], txt: &[char]) -> bool {
1258:     match (pat.first(), txt.first()) {
1259:         (None, None) => true,
1260:         (Some(&'*'), _) => (0..=txt.len()).any(|i| glob_match_impl(&pat[1..], &txt[i..])),
1261:         (Some(&'?'), Some(_)) => glob_match_impl(&pat[1..], &txt[1..]),
1262:         (Some(p), Some(t)) if p == t => glob_match_impl(&pat[1..], &txt[1..]),
1263:         _ => false,
1264:     }
1265: }
1266: 
1267: /// Returns `true` if the system appears to be running on AC power.
1268: /// Best-effort: returns `true` (assume AC) if the check cannot be performed.
1269: fn is_on_ac_power() -> bool {
1270:     // Linux: check /sys/class/power_supply/*/online
1271:     let path = std::path::Path::new("/sys/class/power_supply");
1272:     if !path.exists() {
1273:         return true; // assume AC if sysfs is unavailable
1274:     }
1275:     std::fs::read_dir(path)
1276:         .map(|entries| {
1277:             entries.filter_map(|e| e.ok()).any(|e| {
1278:                 let online = e.path().join("online");
1279:                 std::fs::read_to_string(&online)
1280:                     .map(|s| s.trim() == "1")
1281:                     .unwrap_or(false)
1282:             })
1283:         })
1284:         .unwrap_or(true)
1285: }
1286: 
1287: /// Returns `true` if this appears to be the first boot of the system.
1288: /// Heuristic: `/run/systemd/first-boot` or `/run/machine-id` does not exist.
1289: fn is_first_boot() -> bool {
1290:     std::path::Path::new(common::paths::SYSTEMD_FIRST_BOOT_FILE).exists()
1291: }
1292: 
1293: #[cfg(test)]
1294: mod tests {
1295:     use super::*;
1296:     use crate::state::{Allocator, StartLimitState};
1297:     use std::collections::HashMap;
1298:     use std::time::Duration;
1299: 
1300:     // =========================================================================
1301:     // JobMode tests
1302:     // =========================================================================
1303: 
1304:     #[test]
1305:     fn test_job_mode_from_str_default() {
1306:         assert_eq!(JobMode::from_str("unknown"), JobMode::Replace);
1307:         assert_eq!(JobMode::from_str(""), JobMode::Replace);
1308:     }
1309: 
1310:     #[test]
1311:     fn test_job_mode_from_str_all() {
1312:         assert_eq!(JobMode::from_str("replace"), JobMode::Replace);
1313:         assert_eq!(JobMode::from_str("fail"), JobMode::Fail);
1314:         assert_eq!(JobMode::from_str("queue"), JobMode::Queue);
1315:         assert_eq!(JobMode::from_str("isolate"), JobMode::Isolate);
1316:         assert_eq!(JobMode::from_str("flush"), JobMode::Flush);
1317:         assert_eq!(JobMode::from_str("ignore-dependencies"), JobMode::IgnoreDependencies);
1318:         assert_eq!(JobMode::from_str("ignore-requirements"), JobMode::IgnoreRequirements);
1319:     }
1320: 
1321:     #[test]
1322:     fn test_job_mode_as_str_roundtrip() {
1323:         for mode in &[
1324:             JobMode::Replace,
1325:             JobMode::Fail,
1326:             JobMode::Queue,
1327:             JobMode::Isolate,
1328:             JobMode::Flush,
1329:             JobMode::IgnoreDependencies,
1330:             JobMode::IgnoreRequirements,
1331:         ] {
1332:             assert_eq!(JobMode::from_str(mode.as_str()), *mode);
1333:         }
1334:     }
1335: 
1336:     #[test]
1337:     fn test_job_mode_equality() {
1338:         assert_eq!(JobMode::Replace, JobMode::Replace);
1339:         assert_ne!(JobMode::Replace, JobMode::Fail);
1340:         assert_ne!(JobMode::Isolate, JobMode::Flush);
1341:     }
1342: 
1343:     // =========================================================================
1344:     // StartLimitState tests
1345:     // =========================================================================
1346: 
1347:     #[test]
1348:     fn test_start_limit_state_allows_first_attempt() {
1349:         let mut state = StartLimitState::new();
1350:         assert!(state.check_rate_limit(Duration::from_secs(10), 3));
1351:     }
1352: 
1353:     #[test]
1354:     fn test_start_limit_state_within_burst() {
1355:         let mut state = StartLimitState::new();
1356:         assert!(state.check_rate_limit(Duration::from_secs(10), 3));
1357:         assert!(state.check_rate_limit(Duration::from_secs(10), 3));
1358:         assert!(state.check_rate_limit(Duration::from_secs(10), 3));
1359:     }
1360: 
1361:     #[test]
1362:     fn test_start_limit_state_exceeds_burst() {
1363:         let mut state = StartLimitState::new();
1364:         assert!(state.check_rate_limit(Duration::from_secs(10), 3));
1365:         assert!(state.check_rate_limit(Duration::from_secs(10), 3));
1366:         assert!(state.check_rate_limit(Duration::from_secs(10), 3));
1367:         // The 4th attempt should be rate-limited
1368:         assert!(!state.check_rate_limit(Duration::from_secs(10), 3));
1369:     }
1370: 
1371:     #[test]
1372:     fn test_start_limit_state_prunes_old_timestamps() {
1373:         let mut state = StartLimitState::new();
1374:         // Add some timestamps far in the past
1375:         state.timestamps.push(std::time::Instant::now() - Duration::from_secs(100));
1376:         state.timestamps.push(std::time::Instant::now() - Duration::from_secs(100));
1377:         // With short interval, they should be pruned
1378:         assert!(state.check_rate_limit(Duration::from_secs(1), 5));
1379:         assert_eq!(state.timestamps.len(), 1); // only the new one remains
1380:     }
1381: 
1382:     // =========================================================================

(Output capped at 50 KB. Showing lines 1-1382. Use offset=1383 to continue.)
</content>
```

**Tool: read**

**Input:**
```json
{
  "filePath": "/home/chenpi11/Projects/systema/systema/crates/system-a/src/state.rs"
}
```

**Output:**
```
<path>/home/chenpi11/Projects/systema/systema/crates/system-a/src/state.rs</path>
<type>file</type>
<content>
1: //! Shared mutable state for System A.
2: //!
3: //! `AllocatorState` is the single source of truth for desired unit states,
4: //! registered workers, and in-flight jobs. It is protected by a `RwLock` and
5: //! accessed via `AllocatorHandle` (an `Arc<RwLock<AllocatorState>>`).
6: 
7: use std::collections::HashMap;
8: use std::sync::atomic::{AtomicU64, Ordering};
9: use std::sync::Arc;
10: use std::time::Instant;
11: 
12: use parking_lot::RwLock;
13: use tokio::sync::mpsc;
14: 
15: use crate::unit::types::UnitFile;
16: 
17: // --------------------------------------------------------------------------
18: // Desired state enum
19: // --------------------------------------------------------------------------
20: 
21: /// The state System A wants a unit to be in.
22: #[derive(Debug, Clone, Copy, PartialEq, Eq)]
23: pub enum DesiredState {
24:     Active,
25:     Inactive,
26: }
27: 
28: // --------------------------------------------------------------------------
29: // Job tracking
30: // --------------------------------------------------------------------------
31: 
32: /// The kind of operation a job represents.
33: #[derive(Debug, Clone, Copy, PartialEq, Eq)]
34: pub enum JobKind {
35:     Start,
36:     Stop,
37:     Restart,
38:     Reload,
39: }
40: 
41: impl JobKind {
42:     pub fn as_str(&self) -> &str {
43:         match self {
44:             JobKind::Start => "start",
45:             JobKind::Stop => "stop",
46:             JobKind::Restart => "restart",
47:             JobKind::Reload => "reload",
48:         }
49:     }
50: }
51: 
52: // --------------------------------------------------------------------------
53: // Job mode
54: // --------------------------------------------------------------------------
55: 
56: /// Describes how a job should behave when another job for the same unit
57: /// already exists, and which dependencies to expand.
58: #[derive(Debug, Clone, Copy, PartialEq, Eq)]
59: pub enum JobMode {
60:     /// Replace any existing job for the same unit (default).
61:     Replace,
62:     /// Fail if a job for the same unit already exists.
63:     Fail,
64:     /// Queue behind the existing job.
65:     Queue,
66:     /// Start the unit and stop all other running units.
67:     Isolate,
68:     /// Flush all pending jobs first.
69:     Flush,
70:     /// Start the unit but ignore ordering dependencies.
71:     IgnoreDependencies,
72:     /// Start the unit but ignore requirement dependencies.
73:     IgnoreRequirements,
74: }
75: 
76: impl JobMode {
77:     pub fn from_str(s: &str) -> Self {
78:         match s {
79:             "fail" => JobMode::Fail,
80:             "isolate" => JobMode::Isolate,
81:             "flush" => JobMode::Flush,
82:             "ignore-dependencies" => JobMode::IgnoreDependencies,
83:             "ignore-requirements" => JobMode::IgnoreRequirements,
84:             "queue" => JobMode::Queue,
85:             _ => JobMode::Replace,
86:         }
87:     }
88: 
89:     pub fn as_str(&self) -> &'static str {
90:         match self {
91:             JobMode::Replace => "replace",
92:             JobMode::Fail => "fail",
93:             JobMode::Queue => "queue",
94:             JobMode::Isolate => "isolate",
95:             JobMode::Flush => "flush",
96:             JobMode::IgnoreDependencies => "ignore-dependencies",
97:             JobMode::IgnoreRequirements => "ignore-requirements",
98:         }
99:     }
100: }
101: 
102: /// The current status of a job.
103: #[derive(Debug, Clone, PartialEq, Eq)]
104: pub enum JobStatus {
105:     Waiting,
106:     Running,
107:     Done,
108:     Failed(String),
109:     Cancelled,
110: }
111: 
112: /// A job tracks an in-flight unit operation.
113: #[derive(Debug)]
114: pub struct Job {
115:     pub id: u64,
116:     pub unit_name: String,
117:     pub kind: JobKind,
118:     pub status: JobStatus,
119:     /// One-shot channel used to notify D-Bus callers when the job completes.
120:     pub completion_tx: Option<tokio::sync::oneshot::Sender<JobResult>>,
121: }
122: 
123: /// The result of a job, sent back to the D-Bus caller.
124: #[derive(Debug, Clone)]
125: pub struct JobResult {
126:     pub job_id: u64,
127:     pub unit_name: String,
128:     pub result: JobResultKind,
129: }
130: 
131: /// Notification sent over an internal channel so the D-Bus layer can emit
132: /// the `JobRemoved` signal when a job finishes.
133: #[derive(Debug)]
134: pub struct JobCompletion {
135:     pub job_id: u64,
136:     pub unit_name: String,
137:     pub result: JobResultKind,
138: }
139: 
140: /// Notification sent when a new job is created, so the D-Bus layer can emit
141: /// the `JobNew` signal.
142: #[derive(Debug, Clone)]
143: pub struct JobNewInfo {
144:     pub job_id: u64,
145:     pub unit_name: String,
146:     pub kind: JobKind,
147: }
148: 
149: /// Tracks start attempts for restart rate-limiting.
150: #[derive(Debug, Clone)]
151: pub struct StartLimitState {
152:     /// Timestamps of recent start attempts (within the interval).
153:     pub timestamps: Vec<Instant>,
154: }
155: 
156: impl StartLimitState {
157:     pub fn new() -> Self {
158:         StartLimitState {
159:             timestamps: Vec::new(),
160:         }
161:     }
162: 
163:     /// Prune timestamps older than `interval` and check if the burst limit
164:     /// has been exceeded.
165:     pub fn check_rate_limit(&mut self, interval: std::time::Duration, burst: u32) -> bool {
166:         let now = Instant::now();
167:         self.timestamps.retain(|t| now.duration_since(*t) < interval);
168:         if self.timestamps.len() >= burst as usize {
169:             return false;
170:         }
171:         self.timestamps.push(now);
172:         true
173:     }
174: }
175: 
176: #[derive(Debug, Clone, PartialEq, Eq)]
177: pub enum JobResultKind {
178:     Done,
179:     Failed,
180:     Cancelled,
181:     Timeout,
182:     Dependency,
183:     Skipped,
184: }
185: 
186: impl JobResultKind {
187:     pub fn as_str(&self) -> &str {
188:         match self {
189:             JobResultKind::Done => "done",
190:             JobResultKind::Failed => "failed",
191:             JobResultKind::Cancelled => "canceled",
192:             JobResultKind::Timeout => "timeout",
193:             JobResultKind::Dependency => "dependency",
194:             JobResultKind::Skipped => "skipped",
195:         }
196:     }
197: }
198: 
199: // --------------------------------------------------------------------------
200: // Active unit state (as seen by System A)
201: // --------------------------------------------------------------------------
202: 
203: /// The active state of a unit as last reported by its worker (or inferred).
204: #[derive(Debug, Clone, PartialEq, Eq, Default)]
205: pub enum ActiveState {
206:     #[default]
207:     Inactive,
208:     Activating,
209:     Active,
210:     Deactivating,
211:     Failed,
212:     Reloading,
213: }
214: 
215: impl ActiveState {
216:     pub fn as_str(&self) -> &str {
217:         match self {
218:             ActiveState::Inactive => "inactive",
219:             ActiveState::Activating => "activating",
220:             ActiveState::Active => "active",
221:             ActiveState::Deactivating => "deactivating",
222:             ActiveState::Failed => "failed",
223:             ActiveState::Reloading => "reloading",
224:         }
225:     }
226: }
227: 
228: /// Per-unit runtime info maintained by System A.
229: #[derive(Debug, Clone, Default)]
230: pub struct UnitRuntimeInfo {
231:     pub active_state: ActiveState,
232:     pub sub_state: String,
233:     pub load_state: String,
234:     /// The worker that currently owns this unit (if any).
235:     pub worker_id: Option<String>,
236:     /// The main PID of the service process, if running.
237:     pub main_pid: Option<u32>,
238: }
239: 
240: // --------------------------------------------------------------------------
241: // Worker registry
242: // --------------------------------------------------------------------------
243: 
244: /// A registered System Worker connection.
245: pub struct WorkerEntry {
246:     pub worker_id: String,
247:     /// Unit types this worker handles, e.g. ["service"].
248:     pub unit_types: Vec<String>,
249:     /// Channel to send tasks to this worker's handler task.
250:     pub task_tx: mpsc::Sender<WorkerTask>,
251: }
252: 
253: /// A task sent to a worker.
254: pub struct WorkerTask {
255:     pub task_id: u64,
256:     pub unit_name: String,
257:     pub unit_type: String,
258:     pub kind: JobKind,
259:     pub unit_file: Option<UnitFile>,
260: }
261: 
262: // --------------------------------------------------------------------------
263: // Allocator state
264: // --------------------------------------------------------------------------
265: 
266: pub struct AllocatorState {
267:     /// All loaded unit files.
268:     pub units: HashMap<String, UnitFile>,
269:     /// Desired state for each named unit.
270:     pub desired: HashMap<String, DesiredState>,
271:     /// Runtime info (active state etc.) — updated from worker events.
272:     pub runtime: HashMap<String, UnitRuntimeInfo>,
273:     /// In-flight jobs keyed by job ID.
274:     pub jobs: HashMap<u64, Job>,
275:     /// Registered workers keyed by worker_id.
276:     pub workers: HashMap<String, WorkerEntry>,
277:     /// Maps task_id (IPC level) → JobKind so we can correctly update state
278:     /// when a TaskResult arrives.
279:     pub task_kinds: HashMap<u64, JobKind>,
280:     /// Channel to notify the D-Bus layer when a job completes so it can emit
281:     /// the `JobRemoved` signal.  Set by the D-Bus server at startup.
282:     pub job_completion_tx: Option<tokio::sync::mpsc::UnboundedSender<JobCompletion>>,
283:     /// Channel to notify the D-Bus layer when a new unit is loaded so it can
284:     /// register a per-unit D-Bus object.  Set by the D-Bus server at startup.
285:     pub unit_loaded_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
286:     /// Channel to notify the D-Bus layer when a new job is created so it can
287:     /// emit the `JobNew` signal.
288:     pub job_new_tx: Option<tokio::sync::mpsc::UnboundedSender<JobNewInfo>>,
289:     /// Completion senders for serial task execution — keyed by task_id.
290:     /// When a task completes, `handle_task_result` sends `()` through the
291:     /// corresponding channel so the next task in the serial chain proceeds.
292:     pub serial_completion_txs: HashMap<u64, tokio::sync::oneshot::Sender<()>>,
293:     /// Restart rate-limiting state, keyed by unit name.
294:     pub start_limit_state: HashMap<String, StartLimitState>,
295: }
296: 
297: impl AllocatorState {
298:     pub fn new() -> Self {
299:         AllocatorState {
300:             units: HashMap::new(),
301:             desired: HashMap::new(),
302:             runtime: HashMap::new(),
303:             jobs: HashMap::new(),
304:             workers: HashMap::new(),
305:             task_kinds: HashMap::new(),
306:             job_completion_tx: None,
307:             unit_loaded_tx: None,
308:             job_new_tx: None,
309:             serial_completion_txs: HashMap::new(),
310:             start_limit_state: HashMap::new(),
311:         }
312:     }
313: }
314: 
315: // --------------------------------------------------------------------------
316: // Handle type
317: // --------------------------------------------------------------------------
318: 
319: pub type AllocatorHandle = Arc<RwLock<AllocatorState>>;
320: 
321: /// Wraps `Arc<RwLock<AllocatorState>>` with a convenience constructor.
322: pub struct Allocator;
323: 
324: impl Allocator {
325:     pub fn new() -> AllocatorHandle {
326:         Arc::new(RwLock::new(AllocatorState::new()))
327:     }
328: }
329: 
330: // --------------------------------------------------------------------------
331: // Monotonic ID generator
332: // --------------------------------------------------------------------------
333: 
334: static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);
335: static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);
336: static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
337: 
338: pub fn next_job_id() -> u64 {
339:     NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed)
340: }
341: 
342: pub fn next_task_id() -> u64 {
343:     NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed)
344: }
345: 
346: pub fn next_request_id() -> u64 {
347:     NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
348: }

(End of file - total 348 lines)
</content>
```

---

## Assistant (Plan · DeepSeek V4 Flash Free · 4.5s)

_Thinking:_

Now I have a very thorough understanding of the codebase. Let me also check the main.rs of system-a to understand the startup sequence, and look at the system-s IPC module to understand what events it sends.

**Tool: read**

**Input:**
```json
{
  "filePath": "/home/chenpi11/Projects/systema/systema/crates/system-a/src/main.rs"
}
```

**Output:**
```
<path>/home/chenpi11/Projects/systema/systema/crates/system-a/src/main.rs</path>
<type>file</type>
<content>
1: //! system-a — System Allocator
2: //!
3: //! The global control plane of System Alphabet.  Responsibilities:
4: //! - Load and parse systemd unit files.
5: //! - Build and maintain the unit dependency graph.
6: //! - Generate and dispatch Tasks to System Workers via Unix-socket IPC.
7: //! - Maintain *desired state* for each unit (never actual state).
8: //! - Expose a systemd1-compatible D-Bus interface for external tooling.
9: 
10: mod dbus;
11: mod graph;
12: mod ipc;
13: mod scheduler;
14: mod state;
15: mod unit;
16: 
17: use anyhow::Result;
18: use tracing::info;
19: 
20: #[tokio::main(flavor = "current_thread")]
21: async fn main() -> Result<()> {
22:     // Initialise structured logging.
23:     tracing_subscriber::fmt()
24:         .with_env_filter(
25:             tracing_subscriber::EnvFilter::from_default_env()
26:                 .add_directive("system_a=debug".parse()?)
27:                 .add_directive("common=debug".parse()?),
28:         )
29:         .init();
30: 
31:     info!("System A (System Allocator) starting up");
32: 
33:     // Shared allocator state accessible from both the IPC server and D-Bus server.
34:     let allocator = state::Allocator::new();
35: 
36:     // Load unit files from the default search paths.
37:     unit::loader::load_default_units(allocator.clone()).await?;
38: 
39:     // Start the IPC server (accepts System Worker connections).
40:     let ipc_handle = tokio::spawn(ipc::server::run(allocator.clone()));
41: 
42:     // Start the D-Bus server (exposes systemd1-compatible interface).
43:     let dbus_handle = tokio::spawn(dbus::run(allocator.clone()));
44: 
45:     // Wait for either task to exit (they run indefinitely).
46:     tokio::select! {
47:         res = ipc_handle => {
48:             res??;
49:         }
50:         res = dbus_handle => {
51:             res??;
52:         }
53:     }
54: 
55:     Ok(())
56: }

(End of file - total 56 lines)
</content>
```

**Tool: read**

**Input:**
```json
{
  "filePath": "/home/chenpi11/Projects/systema/systema/crates/system-s/src/ipc/mod.rs"
}
```

**Output:**
```
<path>/home/chenpi11/Projects/systema/systema/crates/system-s/src/ipc/mod.rs</path>
<type>file</type>
<content>
1: //! IPC client for System S.
2: //!
3: //! Connects to System A's Unix socket, registers as a "service" worker, then
4: //! enters a pair of concurrent tasks:
5: //!
6: //! * **reader task**: receives `TaskDispatch` messages, executes them, and
7: //!   queues `TaskResult` messages onto an outgoing channel.
8: //! * **writer task**: drains the outgoing channel and sends messages to
9: //!   System A.
10: //!
11: //! Background monitor tasks (spawned after each `Start`) also push
12: //! `EventPublish` messages onto the same outgoing channel so that unexpected
13: //! service exits are reported to System A without requiring the reader task to
14: //! be idle.
15: 
16: use anyhow::{Context, Result};
17: use bytes::BytesMut;
18: use prost::Message as ProstMessage;
19: use tokio::sync::mpsc;
20: use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
21: use tracing::{debug, info, warn};
22: 
23: use common::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
24: use common::proto::{
25:     Envelope, EventPublish, RegisterAck, TaskDispatch, TaskKind, TaskResult, TaskResultKind,
26:     UnitConfig, WorkerRegistration,
27: };
28: 
29: use crate::process::{start_service, stop_service};
30: use crate::state::{new_registry, ServiceRegistry, ServiceState};
31: 
32: const ALLOCATOR_SOCKET: &str = common::paths::IPC_SOCKET_PATH;
33: const WORKER_ID: &str = "system-s-1";
34: const WORKER_UNIT_TYPES: &[&str] = &["service"];
35: 
36: // ---------------------------------------------------------------------------
37: // Outgoing-message helpers
38: // ---------------------------------------------------------------------------
39: 
40: /// Encode an [`Envelope`] into a length-delimited frame (just the raw bytes;
41: /// the [`FramedWrite`] will prepend the length header).
42: fn encode_envelope(env: Envelope) -> Result<bytes::Bytes> {
43:     let mut buf = BytesMut::new();
44:     env.encode(&mut buf).context("Failed to encode Envelope")?;
45:     Ok(buf.freeze())
46: }
47: 
48: /// Queue an `EventPublish` message for delivery to System A.
49: fn queue_event(
50:     out_tx: &mpsc::UnboundedSender<bytes::Bytes>,
51:     event_type: &str,
52:     unit_name: &str,
53:     data: &[u8],
54: ) -> Result<()> {
55:     let event = EventPublish {
56:         event_type: event_type.to_string(),
57:         unit_name: unit_name.to_string(),
58:         event_data: data.to_vec(),
59:     };
60:     let env = make_envelope(0, WORKER_ID, "system-a", "event.publish", event)?;
61:     out_tx
62:         .send(encode_envelope(env)?)
63:         .map_err(|_| anyhow::anyhow!("Outgoing channel closed"))
64: }
65: 
66: // ---------------------------------------------------------------------------
67: // Public entry-point
68: // ---------------------------------------------------------------------------
69: 
70: /// Connect to System A and run the worker event loop.
71: pub async fn run() -> Result<()> {
72:     let registry = new_registry();
73: 
74:     // Retry connecting to System A with backoff.
75:     let mut backoff = tokio::time::Duration::from_millis(500);
76:     loop {
77:         match try_run(registry.clone()).await {
78:             Ok(()) => {
79:                 info!("Worker loop exited cleanly");
80:                 return Ok(());
81:             }
82:             Err(e) => {
83:                 warn!("Worker error: {}; reconnecting in {:?}", e, backoff);
84:                 tokio::time::sleep(backoff).await;
85:                 backoff = (backoff * 2).min(tokio::time::Duration::from_secs(30));
86:             }
87:         }
88:     }
89: }
90: 
91: // ---------------------------------------------------------------------------
92: // Inner connection loop
93: // ---------------------------------------------------------------------------
94: 
95: async fn try_run(registry: ServiceRegistry) -> Result<()> {
96:     info!("Connecting to System A at {}", ALLOCATOR_SOCKET);
97: 
98:     let stream = tokio::net::UnixStream::connect(ALLOCATOR_SOCKET)
99:         .await
100:         .with_context(|| format!("Cannot connect to {}", ALLOCATOR_SOCKET))?;
101: 
102:     info!("Connected to System A");
103: 
104:     // Use the full framed connection for the registration handshake.
105:     let mut framed = frame_stream(stream);
106: 
107:     // --- Register ---
108:     let reg = WorkerRegistration {
109:         worker_id: WORKER_ID.to_string(),
110:         unit_types: WORKER_UNIT_TYPES.iter().map(|s| s.to_string()).collect(),
111:     };
112:     let env = make_envelope(0, WORKER_ID, "system-a", "worker.register", reg)?;
113:     send_envelope(&mut framed, &env).await?;
114: 
115:     // Wait for ack.
116:     let ack_env = recv_envelope(&mut framed)
117:         .await?
118:         .ok_or_else(|| anyhow::anyhow!("System A closed connection before ack"))?;
119:     let ack = RegisterAck::decode(ack_env.payload.as_slice())?;
120:     if !ack.accepted {
121:         anyhow::bail!("Registration rejected: {}", ack.message);
122:     }
123:     info!("Registration accepted: {}", ack.message);
124: 
125:     // --- Split the connection so reader and writer run concurrently ---
126:     // This lets background monitor tasks push events while the reader
127:     // is blocked inside execute_task waiting for a process to stop.
128:     let inner = framed.into_inner();
129:     let (reader_half, writer_half) = tokio::io::split(inner);
130: 
131:     let make_codec = || {
132:         LengthDelimitedCodec::builder()
133:             .max_frame_length(16 * 1024 * 1024)
134:             .new_codec()
135:     };
136:     let mut reader = FramedRead::new(reader_half, make_codec());
137:     let writer = FramedWrite::new(writer_half, make_codec());
138: 
139:     // Outgoing-message channel: execute_task and monitor_service push encoded
140:     // envelopes here; the writer task drains them onto the socket.
141:     let (out_tx, out_rx) = mpsc::unbounded_channel::<bytes::Bytes>();
142: 
143:     // --- Writer task ---
144:     let writer_task = {
145:         let mut writer = writer;
146:         let mut out_rx = out_rx;
147:         async move {
148:             use futures::SinkExt;
149:             while let Some(msg) = out_rx.recv().await {
150:                 writer.send(msg).await.context("Write to System A socket")?;
151:             }
152:             Ok::<_, anyhow::Error>(())
153:         }
154:     };
155: 
156:     // --- Reader / processor task ---
157:     let reader_task = {
158:         let out_tx = out_tx.clone();
159:         let registry = registry.clone();
160:         async move {
161:             use futures::StreamExt;
162:             loop {
163:                 let bytes = match reader.next().await {
164:                     None => {
165:                         info!("System A closed the connection");
166:                         break;
167:                     }
168:                     Some(Err(e)) => {
169:                         warn!("Read error from System A: {}", e);
170:                         break;
171:                     }
172:                     Some(Ok(b)) => b,
173:                 };
174: 
175:                 let env = match Envelope::decode(bytes.freeze()) {
176:                     Ok(e) => e,
177:                     Err(e) => {
178:                         warn!("Failed to decode envelope: {}", e);
179:                         continue;
180:                     }
181:                 };
182: 
183:                 if env.method != "task.dispatch" {
184:                     warn!("Unexpected method from System A: {}", env.method);
185:                     continue;
186:                 }
187: 
188:                 let task = match TaskDispatch::decode(env.payload.as_slice()) {
189:                     Ok(t) => t,
190:                     Err(e) => {
191:                         warn!("Failed to decode TaskDispatch: {}", e);
192:                         continue;
193:                     }
194:                 };
195:                 debug!(
196:                     "Received task {} for {} (kind={:?})",
197:                     task.task_id, task.unit_name, task.kind
198:                 );
199: 
200:                 let unit_config = if !task.unit_config.is_empty() {
201:                     match UnitConfig::decode(task.unit_config.as_slice()) {
202:                         Ok(c) => Some(c),
203:                         Err(e) => {
204:                             warn!("Failed to decode UnitConfig: {}", e);
205:                             None
206:                         }
207:                     }
208:                 } else {
209:                     None
210:                 };
211: 
212:                 let result = execute_task(&registry, &task, unit_config.as_ref(), &out_tx).await;
213: 
214:                 let (success, message, result_kind) = match result {
215:                     Ok(()) => (true, String::new(), TaskResultKind::TaskResultDone),
216:                     Err(e) => (false, e.to_string(), TaskResultKind::TaskResultFailed),
217:                 };
218: 
219:                 let task_result = TaskResult {
220:                     task_id: task.task_id,
221:                     unit_name: task.unit_name.clone(),
222:                     success,
223:                     message,
224:                     result_kind: result_kind as i32,
225:                 };
226:                 match make_envelope(
227:                     env.request_id,
228:                     WORKER_ID,
229:                     "system-a",
230:                     "task.result",
231:                     task_result,
232:                 )
233:                 .and_then(encode_envelope)
234:                 {
235:                     Ok(encoded) => {
236:                         if out_tx.send(encoded).is_err() {
237:                             warn!("Outgoing channel closed; cannot send task result");
238:                             break;
239:                         }
240:                     }
241:                     Err(e) => {
242:                         warn!("Failed to encode task result: {}", e);
243:                     }
244:                 }
245:             }
246:             Ok::<_, anyhow::Error>(())
247:         }
248:     };
249: 
250:     // Run reader and writer concurrently; stop as soon as either exits.
251:     tokio::select! {
252:         res = writer_task => {
253:             if let Err(e) = res { warn!("Writer task error: {}", e); }
254:         }
255:         res = reader_task => {
256:             if let Err(e) = res { warn!("Reader task error: {}", e); }
257:         }
258:     }
259: 
260:     Ok(())
261: }
262: 
263: // ---------------------------------------------------------------------------
264: // Task execution
265: // ---------------------------------------------------------------------------
266: 
267: /// Execute a single task, queuing events onto `out_tx` instead of writing
268: /// them directly to the socket (so background monitor tasks can share the
269: /// same sender).
270: async fn execute_task(
271:     registry: &ServiceRegistry,
272:     task: &TaskDispatch,
273:     unit_config: Option<&UnitConfig>,
274:     out_tx: &mpsc::UnboundedSender<bytes::Bytes>,
275: ) -> Result<()> {
276:     let kind = TaskKind::try_from(task.kind).unwrap_or(TaskKind::Start);
277: 
278:     match kind {
279:         TaskKind::Start => {
280:             let config = unit_config
281:                 .ok_or_else(|| anyhow::anyhow!("No UnitConfig in task for {}", task.unit_name))?;
282: 
283:             let (pid, child) = start_service(registry.clone(), config).await?;
284: 
285:             // Notify System A that the service is running.
286:             queue_event(
287:                 out_tx,
288:                 "service.started",
289:                 &task.unit_name,
290:                 serde_json::json!({ "pid": pid }).to_string().as_bytes(),
291:             )?;
292: 
293:             // Spawn a background monitor that will detect exit and report
294:             // failures to System A.
295:             tokio::spawn(monitor_service(
296:                 registry.clone(),
297:                 task.unit_name.clone(),
298:                 out_tx.clone(),
299:                 child,
300:             ));
301:         }
302: 
303:         TaskKind::Stop => {
304:             let timeout = unit_config
305:                 .and_then(|c| c.service.as_ref())
306:                 .map(|s| s.timeout_stop_secs)
307:                 .unwrap_or(30);
308: 
309:             stop_service(registry.clone(), &task.unit_name, timeout).await?;
310: 
311:             queue_event(out_tx, "process.exit", &task.unit_name, b"")?;
312:         }
313: 
314:         TaskKind::Restart => {
315:             let timeout = unit_config
316:                 .and_then(|c| c.service.as_ref())
317:                 .map(|s| s.timeout_stop_secs)
318:                 .unwrap_or(30);
319:             stop_service(registry.clone(), &task.unit_name, timeout).await?;
320: 
321:             if let Some(config) = unit_config {
322:                 let (pid, child) = start_service(registry.clone(), config).await?;
323:                 queue_event(
324:                     out_tx,
325:                     "service.started",
326:                     &task.unit_name,
327:                     serde_json::json!({ "pid": pid }).to_string().as_bytes(),
328:                 )?;
329:                 tokio::spawn(monitor_service(
330:                     registry.clone(),
331:                     task.unit_name.clone(),
332:                     out_tx.clone(),
333:                     child,
334:                 ));
335:             }
336:         }
337: 
338:         TaskKind::Reload => {
339:             let pid = {
340:                 let reg = registry.lock();
341:                 reg.get(&task.unit_name).and_then(|i| i.main_pid)
342:             };
343:             if let Some(pid) = pid {
344:                 #[cfg(unix)]
345:                 {
346:                     use nix::sys::signal;
347:                     use nix::unistd::Pid;
348:                     signal::kill(Pid::from_raw(pid as i32), signal::Signal::SIGHUP)
349:                         .context("Failed to send SIGHUP")?;
350:                     info!("Sent SIGHUP to PID {} ({})", pid, task.unit_name);
351:                 }
352:             } else {
353:                 // systemctl reload behaviour: if the service is not running, return an
354:                 // error instead of silently succeeding.
355:                 anyhow::bail!(
356:                     "Reload of {} failed: service is not running",
357:                     task.unit_name
358:                 );
359:             }
360:         }
361:     }
362: 
363:     Ok(())
364: }
365: 
366: // ---------------------------------------------------------------------------
367: // Background monitor
368: // ---------------------------------------------------------------------------
369: 
370: /// Monitor a service process every second and handle exit (expected or
371: /// unexpected).
372: ///
373: /// When the process exits with a non‑zero code the service is marked as
374: /// `Failed` and a `service.failed` event is sent to System A.  A zero exit
375: /// code is treated as a normal termination (`Dead`, not `Failed`).
376: ///
377: /// The task exits when:
378: /// * the service is removed from the registry,
379: /// * an external `Stop` task changes the state to `Stopping`,
380: /// * or the child process exits.
381: async fn monitor_service(
382:     registry: ServiceRegistry,
383:     unit_name: String,
384:     out_tx: mpsc::UnboundedSender<bytes::Bytes>,
385:     mut child: tokio::process::Child,
386: ) {
387:     loop {
388:         tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
389: 
390:         let state = {
391:             let reg = registry.lock();
392:             match reg.get(&unit_name) {
393:                 None => break,
394:                 Some(inst) => inst.state,
395:             }
396:         };
397: 
398:         // Service already marked as stopped / failed / stopping — exit quietly.
399:         if matches!(
400:             state,
401:             ServiceState::Dead | ServiceState::Failed | ServiceState::Stopping
402:         ) {
403:             break;
404:         }
405: 
406:         match child.try_wait() {
407:             Ok(Some(status)) => {
408:                 let pid = child.id().unwrap_or(0);
409:                 info!(
410:                     "Service {} (PID {}) exited: code={:?}, success={}",
411:                     unit_name,
412:                     pid,
413:                     status.code(),
414:                     status.success()
415:                 );
416:                 let state = if status.success() {
417:                     ServiceState::Dead
418:                 } else {
419:                     ServiceState::Failed
420:                 };
421:                 {
422:                     let mut reg = registry.lock();
423:                     if let Some(inst) = reg.get_mut(&unit_name) {
424:                         inst.state = state;
425:                         inst.main_pid = None;
426:                         inst.last_exit_code = status.code();
427:                     }
428:                 }
429:                 if state == ServiceState::Failed {
430:                     // Report the unexpected exit to System A.
431:                     let _ = queue_event(&out_tx, "service.failed", &unit_name, b"");
432:                 }
433:                 break;
434:             }
435:             Ok(None) => {} // still running
436:             Err(e) => {
437:                 warn!("Error waiting for child process of {}: {}", unit_name, e);
438:                 {
439:                     let mut reg = registry.lock();
440:                     if let Some(inst) = reg.get_mut(&unit_name) {
441:                         inst.state = ServiceState::Failed;
442:                         inst.main_pid = None;
443:                     }
444:                 }
445:                 let _ = queue_event(&out_tx, "service.failed", &unit_name, b"");
446:                 break;
447:             }
448:         }
449:     }
450: }

(End of file - total 450 lines)
</content>
```

**Tool: read**

**Input:**
```json
{
  "filePath": "/home/chenpi11/Projects/systema/systema/proto/ipc.proto"
}
```

**Output:**
```
<path>/home/chenpi11/Projects/systema/systema/proto/ipc.proto</path>
<type>file</type>
<content>
1: syntax = "proto3";
2: 
3: package ipc;
4: 
5: // The universal message envelope for all IPC communication between
6: // System A and System Workers.
7: message Envelope {
8:     // Monotonically increasing request ID (0 for one-way events).
9:     uint64 request_id = 1;
10:     // Sender identifier, e.g. "system-a" or "system-s".
11:     string source = 2;
12:     // Recipient identifier, e.g. "system-s" or "system-a".
13:     string target = 3;
14:     // Method/event name, e.g. "task.dispatch", "event.publish", "worker.register".
15:     string method = 4;
16:     // Serialized payload (nested protobuf message).
17:     bytes payload = 5;
18: }
19: 
20: // Sent by a System Worker on connect to declare which unit types it handles.
21: message WorkerRegistration {
22:     // Unique ID of this worker instance, e.g. "system-s-1".
23:     string worker_id = 1;
24:     // Unit types this worker manages, e.g. ["service"].
25:     repeated string unit_types = 2;
26: }
27: 
28: // Sent by System A to a System Worker to request a state transition.
29: message TaskDispatch {
30:     // Unique task ID (correlates with System A job ID).
31:     uint64 task_id = 1;
32:     // The unit this task targets, e.g. "sshd.service".
33:     string unit_name = 2;
34:     // The unit type, e.g. "service".
35:     string unit_type = 3;
36:     // The desired operation.
37:     TaskKind kind = 4;
38:     // Serialized unit configuration (SystemdUnitConfig protobuf).
39:     bytes unit_config = 5;
40: }
41: 
42: enum TaskKind {
43:     TASK_KIND_START = 0;
44:     TASK_KIND_STOP = 1;
45:     TASK_KIND_RESTART = 2;
46:     TASK_KIND_RELOAD = 3;
47: }
48: 
49: // Sent by System Worker back to System A when a task completes or fails.
50: message TaskResult {
51:     // Echoes the task_id from TaskDispatch.
52:     uint64 task_id = 1;
53:     // True if the task succeeded.
54:     bool success = 2;
55:     // Human-readable status message.
56:     string message = 3;
57:     // Structured result code.
58:     TaskResultKind result_kind = 4;
59:     // The unit name the task targeted.
60:     string unit_name = 5;
61: }
62: 
63: enum TaskResultKind {
64:     TASK_RESULT_DONE = 0;
65:     TASK_RESULT_FAILED = 1;
66:     TASK_RESULT_TIMEOUT = 2;
67:     TASK_RESULT_SKIPPED = 3;
68: }
69: 
70: // Published by System Workers for interesting runtime events.
71: message EventPublish {
72:     // Event type string, e.g. "process.exit", "service.started", "service.failed".
73:     string event_type = 1;
74:     // Unit name associated with the event.
75:     string unit_name = 2;
76:     // Optional structured data (JSON for now).
77:     bytes event_data = 3;
78: }
79: 
80: // Sent by System A to acknowledge a worker registration.
81: message RegisterAck {
82:     bool accepted = 1;
83:     string message = 2;
84: }
85: 
86: // Minimal unit configuration sent from System A to System Workers.
87: message UnitConfig {
88:     string unit_name = 1;
89:     string description = 2;
90:     // For service units.
91:     ServiceConfig service = 3;
92: }
93: 
94: message ServiceConfig {
95:     string exec_start = 1;
96:     string exec_stop = 2;
97:     string exec_reload = 3;
98:     string working_directory = 4;
99:     string user = 5;
100:     string group = 6;
101:     repeated string environment = 7;
102:     string restart_policy = 8;   // "no", "on-failure", "always"
103:     uint32 restart_delay_secs = 9;
104:     string service_type = 10;    // "simple", "forking", "oneshot", "notify"
105:     string pid_file = 11;
106:     uint32 timeout_start_secs = 12;
107:     uint32 timeout_stop_secs = 13;
108: }

(End of file - total 108 lines)
</content>
```

---

## Assistant (Plan · DeepSeek V4 Flash Free · 8.9s)

_Thinking:_

Now I have a thorough understanding of the codebase and what Phase 2 requires. Let me summarize my findings and craft a Phase 2 plan.

### What Phase 2 requires (from docs/prompt.md):

1. **事件总线：pub/sub 机制**，System S 发布 `process.exit`，SysA 触发 restart。
2. **重启策略（on-failure, always）**。
3. **System A 能正确从崩溃中恢复**：重读 Unit 文件，与各 System E 恢复连接并同步状态。

### What already exists:

1. **Event publishing**: System S already publishes events (`service.started`, `service.failed`, `process.exit`) via `queue_event()`. System A already handles these events in `handle_event()`.
2. **Restart policy**: Already partially implemented:
   - In `handle_event()` (server.rs:300-365): handles restart for `process.exit` event, checking `RestartPolicy::Always`, `OnFailure`, `OnAbnormal`, `OnAbort`, `OnWatchdog`, `OnSuccess`, with rate limiting.
   - In `handle_task_result()` (scheduler.rs:956-999): handles restart for failed Start tasks, checking `RestartPolicy::Always`, `OnFailure`, `OnAbnormal`.
3. **Crash recovery**: Not implemented at all.

### What's still missing for Phase 2:

1. **真的事件总线 pub/sub 机制** - Currently, events go directly from worker to System A. There's no general pub/sub mechanism where other components could subscribe to events. Need a proper event bus with:
   - Event channels/topics
   - Subscriber registration
   - Broadcast to multiple subscribers
   
2. **重启策略完善** - The restart policy exists but needs:
   - Better handling of restart policy combinations (the current implementation doesn't fully match systemd's behavior for all Restart= combinations)
   - The `process.exit` event in `handle_event` uses hardcoded rate limit values (interval=10s, burst=5) instead of `StartLimitIntervalSec`/`StartLimitBurst` from unit file
   - OnSuccess and other policies may need refinement

3. **System A 崩溃恢复** - Completely missing:
   - After crash, System A needs to reload unit files
   - Workers that reconnect need to have their state synchronized
   - System A needs to reconcile desired state with current state
   - Need a state persistence mechanism (or at least re-discovery of state from workers)

Let me look at what restart policies are defined more carefully, and also check if there are any existing mechanisms for persistence.

Now let me look at the restart policy types and unit file sections to understand what's needed:

**Tool: read**

**Input:**
```json
{
  "filePath": "/home/chenpi11/Projects/systema/systema/crates/system-a/src/unit/types.rs",
  "offset": 1,
  "limit": 100
}
```

**Output:**
```
<path>/home/chenpi11/Projects/systema/systema/crates/system-a/src/unit/types.rs</path>
<type>file</type>
<content>
1: //! Unit file types for System A.
2: //!
3: //! These mirror the relevant subset of systemd unit configuration needed for
4: //! Phase 1 (service management + target activation).
5: 
6: use std::collections::HashSet;
7: 
8: /// The kind of a systemd unit.
9: #[derive(Debug, Clone, PartialEq, Eq, Hash)]
10: pub enum UnitKind {
11:     Service,
12:     Target,
13:     Mount,
14:     Timer,
15:     Socket,
16:     Slice,
17:     Scope,
18:     Swap,
19:     Path,
20:     Device,
21:     Unknown(String),
22: }
23: 
24: impl UnitKind {
25:     pub fn from_extension(name: &str) -> Self {
26:         let ext = name.rsplit('.').next().unwrap_or("");
27:         match ext {
28:             "service" => UnitKind::Service,
29:             "target" => UnitKind::Target,
30:             "mount" => UnitKind::Mount,
31:             "timer" => UnitKind::Timer,
32:             "socket" => UnitKind::Socket,
33:             "slice" => UnitKind::Slice,
34:             "scope" => UnitKind::Scope,
35:             "swap" => UnitKind::Swap,
36:             "path" => UnitKind::Path,
37:             "device" => UnitKind::Device,
38:             other => UnitKind::Unknown(other.to_string()),
39:         }
40:     }
41: 
42:     /// Returns the worker unit-type string used in IPC registration.
43:     pub fn worker_type(&self) -> &str {
44:         match self {
45:             UnitKind::Service => "service",
46:             UnitKind::Target => "target",
47:             UnitKind::Mount => "mount",
48:             UnitKind::Timer => "timer",
49:             UnitKind::Socket => "socket",
50:             UnitKind::Slice => "slice",
51:             UnitKind::Scope => "scope",
52:             UnitKind::Swap => "swap",
53:             UnitKind::Path => "path",
54:             UnitKind::Device => "device",
55:             UnitKind::Unknown(s) => s.as_str(),
56:         }
57:     }
58: }
59: 
60: // ---------------------------------------------------------------------------
61: // ExecCommand
62: // ---------------------------------------------------------------------------
63: 
64: /// A parsed `Exec*=` directive value with all modifier prefix flags extracted.
65: ///
66: /// Systemd supports the following single-character prefixes before the
67: /// executable path, possibly combined in any order:
68: ///
69: /// | Prefix | Meaning |
70: /// |--------|---------|
71: /// | `-`    | Ignore a non-zero exit code (don't mark unit as failed). |
72: /// | `+`    | Run with full privileges (as root, regardless of `User=`/`Group=`). |
73: /// | `@`    | Pass the following token as `argv[0]` to the process. |
74: /// | `:`    | Do not kill the process when the unit stops (no `SIGKILL`). |
75: /// | `!`    | Apply `NoNewPrivileges=yes` to the spawned process. |
76: /// | `!!`   | Like `!`, but only in the seccomp sense (two consecutive `!`). |
77: #[derive(Debug, Clone, PartialEq, Eq)]
78: pub struct ExecCommand {
79:     /// The original value as written in the unit file (including any prefixes).
80:     pub raw: String,
81:     /// Resolved executable path (no prefix modifiers, first word of the command).
82:     pub program: String,
83:     /// Command-line arguments (all words after the program).
84:     pub args: Vec<String>,
85:     /// `-` prefix: do not treat a non-zero exit code as failure.
86:     pub ignore_failure: bool,
87:     /// `+` prefix: run with elevated / root privileges.
88:     pub privileged: bool,
89:     /// `@` prefix: use the next token as `argv[0]` instead of `program`.
90:     pub no_env_lookup: bool,
91:     /// `:` prefix: do not SIGKILL the process when the unit is deactivated.
92:     pub no_kill_on_stop: bool,
93:     /// `!` or `!!` prefix: apply `NoNewPrivileges`.
94:     pub no_new_privileges: bool,
95: }
96: 
97: impl ExecCommand {
98:     /// Parse a raw `Exec*=` line, stripping modifier prefixes and splitting
99:     /// the remaining text into `program` + `args`.
100:     pub fn parse(raw: impl Into<String>) -> Self {

(Showing lines 1-100 of 860. Use offset=101 to continue.)
</content>
```

**Tool: read**

**Input:**
```json
{
  "filePath": "/home/chenpi11/Projects/systema/systema/crates/system-a/src/unit/types.rs",
  "offset": 100,
  "limit": 200
}
```

**Output:**
```
<path>/home/chenpi11/Projects/systema/systema/crates/system-a/src/unit/types.rs</path>
<type>file</type>
<content>
100:     pub fn parse(raw: impl Into<String>) -> Self {
101:         let raw: String = raw.into();
102:         let trimmed = raw.trim_start();
103: 
104:         let mut cursor = trimmed;
105:         let mut ignore_failure = false;
106:         let mut privileged = false;
107:         let mut no_env_lookup = false;
108:         let mut no_kill_on_stop = false;
109:         let mut no_new_privileges = false;
110: 
111:         // Collect prefix characters until we hit something that isn't one.
112:         loop {
113:             match cursor.as_bytes().first() {
114:                 Some(b'-') => {
115:                     ignore_failure = true;
116:                     cursor = &cursor[1..];
117:                 }
118:                 Some(b'+') => {
119:                     privileged = true;
120:                     cursor = &cursor[1..];
121:                 }
122:                 Some(b'@') => {
123:                     no_env_lookup = true;
124:                     cursor = &cursor[1..];
125:                 }
126:                 Some(b':') => {
127:                     no_kill_on_stop = true;
128:                     cursor = &cursor[1..];
129:                 }
130:                 Some(b'!') => {
131:                     no_new_privileges = true;
132:                     cursor = &cursor[1..];
133:                 }
134:                 _ => break,
135:             }
136:         }
137: 
138:         let parts = shell_words(cursor);
139:         let program = parts.first().cloned().unwrap_or_default();
140:         let args = parts.into_iter().skip(1).collect();
141: 
142:         ExecCommand {
143:             raw,
144:             program,
145:             args,
146:             ignore_failure,
147:             privileged,
148:             no_env_lookup,
149:             no_kill_on_stop,
150:             no_new_privileges,
151:         }
152:     }
153: 
154:     /// Returns a plain command string (program + space-joined args) without
155:     /// any modifier prefixes.  Suitable for display or for passing to a shell.
156:     pub fn command_line(&self) -> String {
157:         if self.args.is_empty() {
158:             self.program.clone()
159:         } else {
160:             format!("{} {}", self.program, self.args.join(" "))
161:         }
162:     }
163: }
164: 
165: // ---------------------------------------------------------------------------
166: // Shell-word splitter (private helper used by ExecCommand::parse)
167: // ---------------------------------------------------------------------------
168: 
169: /// Split a command-line string into words, respecting single and double
170: /// quotes and backslash escapes.
171: fn shell_words(s: &str) -> Vec<String> {
172:     let mut words: Vec<String> = Vec::new();
173:     let mut current = String::new();
174:     let mut in_single = false;
175:     let mut in_double = false;
176:     let mut escaped = false;
177: 
178:     for ch in s.chars() {
179:         if escaped {
180:             current.push(ch);
181:             escaped = false;
182:             continue;
183:         }
184:         match ch {
185:             '\\' if !in_single => {
186:                 escaped = true;
187:             }
188:             '\'' if !in_double => {
189:                 in_single = !in_single;
190:             }
191:             '"' if !in_single => {
192:                 in_double = !in_double;
193:             }
194:             ' ' | '\t' if !in_single && !in_double => {
195:                 if !current.is_empty() {
196:                     words.push(current.drain(..).collect());
197:                 }
198:             }
199:             other => {
200:                 current.push(other);
201:             }
202:         }
203:     }
204:     if !current.is_empty() {
205:         words.push(current);
206:     }
207:     words
208: }
209: 
210: // ---------------------------------------------------------------------------
211: // [Unit] section
212: // ---------------------------------------------------------------------------
213: 
214: /// Common `[Unit]` section fields shared by all unit types.
215: #[derive(Debug, Clone, Default)]
216: pub struct UnitSection {
217:     pub description: String,
218:     pub documentation: Vec<String>,
219:     /// Units that must be active before this unit can start.
220:     pub requires: HashSet<String>,
221:     /// Units that should be active before this unit starts (non-fatal).
222:     pub wants: HashSet<String>,
223:     /// Units that conflict with this unit.
224:     pub conflicts: HashSet<String>,
225:     /// Ordering: start after these units.
226:     pub after: HashSet<String>,
227:     /// Ordering: start before these units.
228:     pub before: HashSet<String>,
229:     /// If these units are stopped, also stop this unit.
230:     pub part_of: HashSet<String>,
231:     /// Bind the lifecycle to these units (if they stop, stop this one).
232:     pub binds_to: HashSet<String>,
233:     /// Like Requires but the dependency must already be active (not started).
234:     pub requisite: HashSet<String>,
235:     /// Continuously maintain activation of these units.
236:     pub upholds: HashSet<String>,
237:     /// Units to activate when this unit succeeds.
238:     pub on_success: HashSet<String>,
239:     /// Units to activate when this unit fails.
240:     pub on_failure: HashSet<String>,
241:     /// When this unit is reloaded, also reload these units.
242:     pub propagates_reload_to: HashSet<String>,
243:     pub default_dependencies: bool,
244: 
245:     // ------------------------------------------------------------------
246:     // Condition checks — parsed; evaluated by the scheduler before start.
247:     // ------------------------------------------------------------------
248: 
249:     /// `ConditionPathExists=` — skip start if path does not exist.
250:     /// A `!`-prefixed value negates the check.
251:     pub condition_path_exists: Vec<String>,
252:     /// `ConditionPathExistsGlob=` — skip start if no path matches the glob.
253:     pub condition_path_exists_glob: Vec<String>,
254:     /// `ConditionFileNotEmpty=` — skip start if file is missing or empty.
255:     pub condition_file_not_empty: Vec<String>,
256:     /// `ConditionDirectoryNotEmpty=` — skip start if directory is empty/missing.
257:     pub condition_directory_not_empty: Vec<String>,
258:     /// `ConditionHost=` — match against hostname or machine ID.
259:     pub condition_host: Vec<String>,
260:     /// `ConditionKernelCommandLine=` — match against `/proc/cmdline` token.
261:     pub condition_kernel_command_line: Vec<String>,
262:     /// `ConditionVirtualization=` — e.g. `no`, `yes`, `kvm`, `docker`.
263:     pub condition_virtualization: Vec<String>,
264:     /// `ConditionSecurity=` — e.g. `selinux`, `apparmor`, `ima`.
265:     pub condition_security: Vec<String>,
266:     /// `ConditionCapability=` — kernel capability name.
267:     pub condition_capability: Vec<String>,
268:     /// `ConditionACPower=` — `yes` or `no`.
269:     pub condition_ac_power: Vec<String>,
270:     /// `ConditionNeedsUpdate=` — path to check for updates (e.g. `/etc`).
271:     pub condition_needs_update: Vec<String>,
272:     /// `ConditionFirstBoot=` — `yes` or `no`.
273:     pub condition_first_boot: Vec<String>,
274:     /// `ConditionEnvironment=` — environment variable (optionally `=value`).
275:     pub condition_environment: Vec<String>,
276:     /// `ConditionMemory=` — memory size comparison, e.g. `>=1G`.
277:     pub condition_memory: Vec<String>,
278: 
279:     // ------------------------------------------------------------------
280:     // Assert checks — like Condition but cause a hard failure if not met.
281:     // ------------------------------------------------------------------
282: 
283:     /// `AssertPathExists=`
284:     pub assert_path_exists: Vec<String>,
285:     /// `AssertPathExistsGlob=`
286:     pub assert_path_exists_glob: Vec<String>,
287:     /// `AssertFileNotEmpty=`
288:     pub assert_file_not_empty: Vec<String>,
289:     /// `AssertDirectoryNotEmpty=`
290:     pub assert_directory_not_empty: Vec<String>,
291:     /// `AssertHost=`
292:     pub assert_host: Vec<String>,
293:     /// `AssertFirstBoot=`
294:     pub assert_first_boot: Vec<String>,
295:     /// `AssertMemory=`
296:     pub assert_memory: Vec<String>,
297: }
298: 
299: // ---------------------------------------------------------------------------

(Showing lines 100-299 of 860. Use offset=300 to continue.)
</content>
```

**Tool: read**

**Input:**
```json
{
  "filePath": "/home/chenpi11/Projects/systema/systema/crates/system-a/src/unit/types.rs",
  "offset": 300,
  "limit": 200
}
```

**Output:**
```
<path>/home/chenpi11/Projects/systema/systema/crates/system-a/src/unit/types.rs</path>
<type>file</type>
<content>
300: // [Install] section
301: // ---------------------------------------------------------------------------
302: 
303: /// `[Install]` section.
304: #[derive(Debug, Clone, Default)]
305: pub struct InstallSection {
306:     /// Targets that want this unit (used for enable/disable).
307:     pub wanted_by: HashSet<String>,
308:     pub required_by: HashSet<String>,
309:     pub also: HashSet<String>,
310:     pub alias: Vec<String>,
311: }
312: 
313: // ---------------------------------------------------------------------------
314: // [Service] section
315: // ---------------------------------------------------------------------------
316: 
317: /// `[Service]` section.
318: #[derive(Debug, Clone, Default)]
319: pub struct ServiceSection {
320:     pub service_type: ServiceType,
321:     pub exec_start: Vec<ExecCommand>,
322:     pub exec_start_pre: Vec<ExecCommand>,
323:     pub exec_start_post: Vec<ExecCommand>,
324:     pub exec_stop: Vec<ExecCommand>,
325:     pub exec_stop_post: Vec<ExecCommand>,
326:     pub exec_reload: Vec<ExecCommand>,
327:     pub working_directory: String,
328:     pub user: String,
329:     pub group: String,
330:     pub environment: Vec<String>,
331:     pub environment_file: Vec<String>,
332:     pub pid_file: String,
333:     pub restart: RestartPolicy,
334:     pub restart_sec: u32,
335:     pub timeout_start_sec: u32,
336:     pub timeout_stop_sec: u32,
337:     pub remain_after_exit: bool,
338:     pub bus_name: String,
339:     pub notify_access: String,
340:     pub watchdog_sec: u32,
341:     pub kill_signal: String,
342:     pub kill_mode: String,
343:     pub standard_output: String,
344:     pub standard_error: String,
345: }
346: 
347: #[derive(Debug, Clone, PartialEq, Eq, Default)]
348: pub enum ServiceType {
349:     #[default]
350:     Simple,
351:     Forking,
352:     Oneshot,
353:     Dbus,
354:     Notify,
355:     NotifyReload,
356:     Idle,
357: }
358: 
359: impl ServiceType {
360:     pub fn as_str(&self) -> &str {
361:         match self {
362:             ServiceType::Simple => "simple",
363:             ServiceType::Forking => "forking",
364:             ServiceType::Oneshot => "oneshot",
365:             ServiceType::Dbus => "dbus",
366:             ServiceType::Notify => "notify",
367:             ServiceType::NotifyReload => "notify-reload",
368:             ServiceType::Idle => "idle",
369:         }
370:     }
371: }
372: 
373: impl From<&str> for ServiceType {
374:     fn from(s: &str) -> Self {
375:         match s.to_lowercase().as_str() {
376:             "simple" => ServiceType::Simple,
377:             "forking" => ServiceType::Forking,
378:             "oneshot" => ServiceType::Oneshot,
379:             "dbus" => ServiceType::Dbus,
380:             "notify" => ServiceType::Notify,
381:             "notify-reload" => ServiceType::NotifyReload,
382:             "idle" => ServiceType::Idle,
383:             _ => ServiceType::Simple,
384:         }
385:     }
386: }
387: 
388: #[derive(Debug, Clone, PartialEq, Eq, Default)]
389: pub enum RestartPolicy {
390:     #[default]
391:     No,
392:     OnSuccess,
393:     OnFailure,
394:     OnAbnormal,
395:     OnWatchdog,
396:     OnAbort,
397:     Always,
398: }
399: 
400: impl RestartPolicy {
401:     pub fn as_str(&self) -> &str {
402:         match self {
403:             RestartPolicy::No => "no",
404:             RestartPolicy::OnSuccess => "on-success",
405:             RestartPolicy::OnFailure => "on-failure",
406:             RestartPolicy::OnAbnormal => "on-abnormal",
407:             RestartPolicy::OnWatchdog => "on-watchdog",
408:             RestartPolicy::OnAbort => "on-abort",
409:             RestartPolicy::Always => "always",
410:         }
411:     }
412: }
413: 
414: impl From<&str> for RestartPolicy {
415:     fn from(s: &str) -> Self {
416:         match s.to_lowercase().as_str() {
417:             "on-success" => RestartPolicy::OnSuccess,
418:             "on-failure" => RestartPolicy::OnFailure,
419:             "on-abnormal" => RestartPolicy::OnAbnormal,
420:             "on-watchdog" => RestartPolicy::OnWatchdog,
421:             "on-abort" => RestartPolicy::OnAbort,
422:             "always" => RestartPolicy::Always,
423:             _ => RestartPolicy::No,
424:         }
425:     }
426: }
427: 
428: /// A fully parsed systemd unit file.
429: #[derive(Debug, Clone)]
430: pub struct UnitFile {
431:     /// The canonical unit name, e.g. "sshd.service".
432:     pub name: String,
433:     pub kind: UnitKind,
434:     pub unit: UnitSection,
435:     pub install: InstallSection,
436:     /// Present only for service units.
437:     pub service: Option<ServiceSection>,
438:     /// Present only for mount units.
439:     pub mount: Option<MountSection>,
440:     /// Present only for timer units.
441:     pub timer: Option<TimerSection>,
442:     /// Present only for socket units.
443:     pub socket: Option<SocketSection>,
444:     /// Present only for swap units.
445:     pub swap: Option<SwapSection>,
446:     /// Present only for path units.
447:     pub path: Option<PathSection>,
448:     /// Present only for slice units.
449:     pub slice: Option<SliceSection>,
450:     /// Present only for scope units.
451:     pub scope: Option<ScopeSection>,
452:     /// Present only for device units.
453:     pub device: Option<DeviceSection>,
454: }
455: 
456: impl UnitFile {
457:     pub fn new(name: impl Into<String>) -> Self {
458:         let name = name.into();
459:         let kind = UnitKind::from_extension(&name);
460:         UnitFile {
461:             name,
462:             kind,
463:             unit: UnitSection::default(),
464:             install: InstallSection::default(),
465:             service: None,
466:             mount: None,
467:             timer: None,
468:             socket: None,
469:             swap: None,
470:             path: None,
471:             slice: None,
472:             scope: None,
473:             device: None,
474:         }
475:     }
476: }
477: 
478: // ---------------------------------------------------------------------------
479: // [Mount] section
480: // ---------------------------------------------------------------------------
481: 
482: /// `[Mount]` section for `.mount` units.
483: #[derive(Debug, Clone, Default)]
484: pub struct MountSection {
485:     /// Device or remote filesystem to mount (`What=`).
486:     pub what: String,
487:     /// Mount point path (`Where=`).
488:     pub where_: String,
489:     /// Filesystem type, e.g. `ext4`, `nfs` (`Type=`).
490:     pub type_: String,
491:     /// Mount options passed to `mount(8)` (`Options=`).
492:     pub options: String,
493:     /// Time to wait for the mount operation in seconds (`TimeoutSec=`).
494:     pub timeout_sec: u32,
495:     /// Unmount lazily when the mount point becomes unreachable (`LazyUnmount=`).
496:     pub lazy_unmount: bool,
497:     /// Force unmount even if the filesystem is busy (`ForceUnmount=`).
498:     pub force_unmount: bool,
499:     /// Permission mode for the mount point directory (`DirectoryMode=`).

(Showing lines 300-499 of 860. Use offset=500 to continue.)
</content>
```

---

## Assistant (Plan · DeepSeek V4 Flash Free · 4.0s)

_Thinking:_

Now I have a comprehensive understanding. Let me also check the remaining parts of the types.rs to see the TimerSection, and check the docs/allocator-todo.md for any existing TODO items for Phase 2.

**Tool: read**

**Input:**
```json
{
  "filePath": "/home/chenpi11/Projects/systema/systema/crates/system-a/src/unit/types.rs",
  "offset": 500
}
```

**Output:**
```
<path>/home/chenpi11/Projects/systema/systema/crates/system-a/src/unit/types.rs</path>
<type>file</type>
<content>
500:     pub directory_mode: String,
501:     /// Ignore unknown mount options (`SloppyOptions=`).
502:     pub sloppy_options: bool,
503: }
504: 
505: // ---------------------------------------------------------------------------
506: // [Timer] section
507: // ---------------------------------------------------------------------------
508: 
509: /// `[Timer]` section for `.timer` units.
510: #[derive(Debug, Clone, Default)]
511: pub struct TimerSection {
512:     /// Run N seconds after the timer unit was activated (`OnActiveSec=`).
513:     pub on_active_sec: Option<u32>,
514:     /// Run N seconds after boot (`OnBootSec=`).
515:     pub on_boot_sec: Option<u32>,
516:     /// Run N seconds after systemd started (`OnStartupSec=`).
517:     pub on_startup_sec: Option<u32>,
518:     /// Run N seconds after the activated unit last became active (`OnUnitActiveSec=`).
519:     pub on_unit_active_sec: Option<u32>,
520:     /// Run N seconds after the activated unit last became inactive (`OnUnitInactiveSec=`).
521:     pub on_unit_inactive_sec: Option<u32>,
522:     /// Calendar-based schedules, e.g. `"*-*-* 02:00:00"` (`OnCalendar=`).
523:     pub on_calendar: Vec<String>,
524:     /// Accuracy of the timer in seconds (`AccuracySec=`; default 60).
525:     pub accuracy_sec: u32,
526:     /// Randomised delay added to the trigger time (`RandomizedDelaySec=`).
527:     pub randomized_delay_sec: u32,
528:     /// Unit to activate (default: same stem with `.service` suffix) (`Unit=`).
529:     pub unit: String,
530:     /// Save last trigger time across reboots (`Persistent=`).
531:     pub persistent: bool,
532:     /// Wake the system from suspend to fire the timer (`WakeSystem=`).
533:     pub wake_system: bool,
534:     /// Keep the timer active after it has elapsed once (`RemainAfterElapse=`).
535:     pub remain_after_elapse: bool,
536: }
537: 
538: // ---------------------------------------------------------------------------
539: // [Socket] section
540: // ---------------------------------------------------------------------------
541: 
542: /// `[Socket]` section for `.socket` units.
543: #[derive(Debug, Clone, Default)]
544: pub struct SocketSection {
545:     /// Stream (TCP / Unix stream) listening addresses (`ListenStream=`).
546:     pub listen_stream: Vec<String>,
547:     /// Datagram (UDP / Unix datagram) listening addresses (`ListenDatagram=`).
548:     pub listen_datagram: Vec<String>,
549:     /// Sequential-packet Unix socket paths (`ListenSequentialPacket=`).
550:     pub listen_sequential_packet: Vec<String>,
551:     /// FIFO paths to create (`ListenFIFO=`).
552:     pub listen_fifo: Vec<String>,
553:     /// Netlink socket families (`ListenNetlink=`).
554:     pub listen_netlink: Vec<String>,
555:     /// Special file paths (`ListenSpecial=`).
556:     pub listen_special: Vec<String>,
557:     /// Accept a connection per service instance (`Accept=`).
558:     pub accept: bool,
559:     /// User for socket file ownership (`SocketUser=`).
560:     pub socket_user: String,
561:     /// Group for socket file ownership (`SocketGroup=`).
562:     pub socket_group: String,
563:     /// Socket file permission mode, e.g. `0666` (`SocketMode=`).
564:     pub socket_mode: String,
565:     /// Permission mode for automatically created socket directories (`DirectoryMode=`).
566:     pub directory_mode: String,
567:     /// Maximum number of concurrent connections (`MaxConnections=`).
568:     pub max_connections: u32,
569:     /// Listen backlog (`Backlog=`).
570:     pub backlog: u32,
571:     /// IPv6-only binding: `default`, `both`, or `ipv6-only` (`BindIPv6Only=`).
572:     pub bind_ipv6_only: String,
573:     /// Bind to a non-local address (`FreeBind=`).
574:     pub free_bind: bool,
575:     /// Enable `IP_TRANSPARENT` (`Transparent=`).
576:     pub transparent: bool,
577:     /// Enable `SO_BROADCAST` (`Broadcast=`).
578:     pub broadcast: bool,
579:     /// Pass SCM_CREDENTIALS ancillary data (`PassCredentials=`).
580:     pub pass_credentials: bool,
581:     /// Pass SCM_SECURITY ancillary data (`PassSecurity=`).
582:     pub pass_security: bool,
583:     /// Time to wait for socket activation in seconds (`TimeoutSec=`).
584:     pub timeout_sec: u32,
585: }
586: 
587: // ---------------------------------------------------------------------------
588: // [Swap] section
589: // ---------------------------------------------------------------------------
590: 
591: /// `[Swap]` section for `.swap` units.
592: #[derive(Debug, Clone)]
593: pub struct SwapSection {
594:     /// Swap device or file (`What=`).
595:     pub what: String,
596:     /// Swap priority (`Priority=`; default -1 meaning kernel default).
597:     pub priority: i32,
598:     /// Options passed to `swapon(8)` (`Options=`).
599:     pub options: String,
600:     /// Time to wait for the swap operation in seconds (`TimeoutSec=`).
601:     pub timeout_sec: u32,
602: }
603: 
604: impl Default for SwapSection {
605:     fn default() -> Self {
606:         SwapSection {
607:             what: String::new(),
608:             priority: -1,
609:             options: String::new(),
610:             timeout_sec: 90,
611:         }
612:     }
613: }
614: 
615: // ---------------------------------------------------------------------------
616: // [Slice] section
617: // ---------------------------------------------------------------------------
618: 
619: /// `[Slice]` section for `.slice` units.
620: ///
621: /// Slices are cgroup-based resource management units. They don't have
622: /// their own processes but group other units (services, scopes, etc.)
623: /// for resource control.
624: #[derive(Debug, Clone, Default)]
625: pub struct SliceSection {
626:     /// CPU quota relative to the total CPU time (`CPUQuota=`).
627:     /// e.g. "50%" means the slice can use at most 50% of one CPU.
628:     pub cpu_quota: String,
629:     /// CPU weight (relative weight, default 100) (`CPUWeight=`).
630:     pub cpu_weight: u32,
631:     /// Startup CPU weight (`StartupCPUWeight=`).
632:     pub startup_cpu_weight: u32,
633:     /// CPU set (`CPUSetCPUs=`, `CPUSetMemoryNodes=`).
634:     pub cpu_set_cpus: String,
635:     pub cpu_set_memory_nodes: String,
636:     /// Memory limit (`MemoryMax=`, `MemoryHigh=`, `MemoryLow=`, `MemoryMin=`).
637:     pub memory_max: String,
638:     pub memory_high: String,
639:     pub memory_low: String,
640:     pub memory_min: String,
641:     /// IO weight (`IOWeight=`).
642:     pub io_weight: u32,
643:     /// IO bandwidth limits (`IOBandwidthMax=`).
644:     pub io_bandwidth_max: String,
645:     /// Tasks max (`TasksMax=`).
646:     pub tasks_max: u32,
647:     /// Allowed CPUs (`AllowedCPUs=`).
648:     pub allowed_cpus: String,
649:     /// Allowed memory nodes (`AllowedMemoryNodes=`).
650:     pub allowed_memory_nodes: String,
651: }
652: 
653: // ---------------------------------------------------------------------------
654: // [Scope] section
655: // ---------------------------------------------------------------------------
656: 
657: /// `[Scope]` section for `.scope` units.
658: ///
659: /// Scopes are cgroup-based units that wrap externally created processes
660: /// (not spawned by systemd). They are used for resource management of
661: /// processes started by other means.
662: #[derive(Debug, Clone, Default)]
663: pub struct ScopeSection {
664:     /// Processes to include in the scope (`PIDs=`).
665:     pub pids: Vec<String>,
666:     /// Timeout for the scope in seconds (`TimeoutStopSec=`).
667:     pub timeout_stop_sec: u32,
668:     /// Runtime timeout in seconds (`RuntimeMaxSec=`).
669:     pub runtime_max_sec: u32,
670:     /// Whether to kill all processes when the scope is stopped (`KillMode=`).
671:     pub kill_mode: String,
672:     /// Signal to send when stopping (`KillSignal=`).
673:     pub kill_signal: String,
674:     /// Whether to send SIGHUP before SIGKILL (`SendSIGHUP=`).
675:     pub send_sighup: bool,
676:     /// Resource limits (same as Slice).
677:     pub cpu_quota: String,
678:     pub cpu_weight: u32,
679:     pub memory_max: String,
680:     pub tasks_max: u32,
681: }
682: 
683: // ---------------------------------------------------------------------------
684: // [Device] section
685: // ---------------------------------------------------------------------------
686: 
687: /// `[Device]` section for `.device` units.
688: ///
689: /// Device units represent device files in `/dev/`. They are typically
690: /// created automatically by systemd when devices appear, but can also
691: /// be defined in unit files for udev rule integration.
692: #[derive(Debug, Clone, Default)]
693: pub struct DeviceSection {
694:     /// udev property name to match (`Property=`).
695:     pub property: Vec<String>,
696:     /// sysfs path to match (`SysfsPath=`).
697:     pub sysfs_path: String,
698:     /// Device name pattern (`DeviceName=`).
699:     pub device_name: String,
700:     /// Shell-style pattern for device name (`DevicePath=`).
701:     pub device_path: String,
702: }
703: 
704: // ---------------------------------------------------------------------------
705: // [Path] section
706: // ---------------------------------------------------------------------------
707: 
708: /// `[Path]` section for `.path` units.
709: #[derive(Debug, Clone, Default)]
710: pub struct PathSection {
711:     /// Activate when the path exists (`PathExists=`).
712:     pub path_exists: Vec<String>,
713:     /// Activate when a path matching the glob exists (`PathExistsGlob=`).
714:     pub path_exists_glob: Vec<String>,
715:     /// Activate when the path is created or modified (`PathChanged=`).
716:     pub path_changed: Vec<String>,
717:     /// Activate when the path is modified (`PathModified=`).
718:     pub path_modified: Vec<String>,
719:     /// Activate when the directory is non-empty (`DirectoryNotEmpty=`).
720:     pub directory_not_empty: Vec<String>,
721:     /// Unit to activate (default: same stem with `.service` suffix) (`Unit=`).
722:     pub unit: String,
723:     /// Create watched directory if missing (`MakeDirectory=`).
724:     pub make_directory: bool,
725:     /// Permission mode for auto-created directories (`DirectoryMode=`).
726:     pub directory_mode: String,
727:     /// Rate-limit interval in seconds (`TriggerLimitIntervalSec=`).
728:     pub trigger_limit_interval_sec: u32,
729:     /// Burst count for the rate limiter (`TriggerLimitBurst=`).
730:     pub trigger_limit_burst: u32,
731: }
732: 
733: #[cfg(test)]
734: mod tests {
735:     use super::*;
736: 
737:     #[test]
738:     fn classifies_extended_unit_kinds() {
739:         assert_eq!(UnitKind::from_extension("demo.socket"), UnitKind::Socket);
740:         assert_eq!(UnitKind::from_extension("demo.slice"), UnitKind::Slice);
741:         assert_eq!(UnitKind::from_extension("demo.scope"), UnitKind::Scope);
742:     }
743: 
744:     #[test]
745:     fn classifies_new_unit_kinds() {
746:         assert_eq!(UnitKind::from_extension("dev-sda1.swap"), UnitKind::Swap);
747:         assert_eq!(UnitKind::from_extension("watch.path"), UnitKind::Path);
748:         assert_eq!(UnitKind::from_extension("dev-sda.device"), UnitKind::Device);
749:     }
750: 
751:     #[test]
752:     fn worker_type_round_trips() {
753:         assert_eq!(UnitKind::Swap.worker_type(), "swap");
754:         assert_eq!(UnitKind::Path.worker_type(), "path");
755:         assert_eq!(UnitKind::Device.worker_type(), "device");
756:     }
757: 
758:     #[test]
759:     fn exec_command_no_prefix() {
760:         let cmd = ExecCommand::parse("/usr/sbin/sshd -D");
761:         assert_eq!(cmd.program, "/usr/sbin/sshd");
762:         assert_eq!(cmd.args, vec!["-D"]);
763:         assert!(!cmd.ignore_failure);
764:         assert!(!cmd.privileged);
765:         assert!(!cmd.no_env_lookup);
766:         assert!(!cmd.no_kill_on_stop);
767:         assert!(!cmd.no_new_privileges);
768:         assert_eq!(cmd.raw, "/usr/sbin/sshd -D");
769:     }
770: 
771:     #[test]
772:     fn exec_command_ignore_failure_prefix() {
773:         let cmd = ExecCommand::parse("-/usr/bin/cleanup");
774:         assert!(cmd.ignore_failure);
775:         assert_eq!(cmd.program, "/usr/bin/cleanup");
776:         assert!(cmd.args.is_empty());
777:     }
778: 
779:     #[test]
780:     fn exec_command_privileged_prefix() {
781:         let cmd = ExecCommand::parse("+/usr/sbin/chown root /run/x");
782:         assert!(cmd.privileged);
783:         assert_eq!(cmd.program, "/usr/sbin/chown");
784:         assert_eq!(cmd.args, vec!["root", "/run/x"]);
785:     }
786: 
787:     #[test]
788:     fn exec_command_at_prefix() {
789:         let cmd = ExecCommand::parse("@/usr/lib/foo/foo argv0 arg1");
790:         assert!(cmd.no_env_lookup);
791:         assert_eq!(cmd.program, "/usr/lib/foo/foo");
792:     }
793: 
794:     #[test]
795:     fn exec_command_no_kill_prefix() {
796:         let cmd = ExecCommand::parse(":/usr/bin/bar");
797:         assert!(cmd.no_kill_on_stop);
798:     }
799: 
800:     #[test]
801:     fn exec_command_no_new_privileges_prefix() {
802:         let cmd = ExecCommand::parse("!/usr/bin/baz");
803:         assert!(cmd.no_new_privileges);
804:         assert_eq!(cmd.program, "/usr/bin/baz");
805:     }
806: 
807:     #[test]
808:     fn exec_command_double_bang_prefix() {
809:         let cmd = ExecCommand::parse("!!/usr/bin/qux");
810:         assert!(cmd.no_new_privileges);
811:         assert_eq!(cmd.program, "/usr/bin/qux");
812:     }
813: 
814:     #[test]
815:     fn exec_command_combined_prefixes() {
816:         let cmd = ExecCommand::parse("-+/usr/bin/cmd arg1 arg2");
817:         assert!(cmd.ignore_failure);
818:         assert!(cmd.privileged);
819:         assert_eq!(cmd.program, "/usr/bin/cmd");
820:         assert_eq!(cmd.args, vec!["arg1", "arg2"]);
821:     }
822: 
823:     #[test]
824:     fn exec_command_empty() {
825:         let cmd = ExecCommand::parse("");
826:         assert_eq!(cmd.program, "");
827:         assert!(cmd.args.is_empty());
828:     }
829: 
830:     #[test]
831:     fn exec_command_quoted_args() {
832:         let cmd = ExecCommand::parse("/usr/bin/cmd 'hello world' \"foo bar\"");
833:         assert_eq!(cmd.program, "/usr/bin/cmd");
834:         assert_eq!(cmd.args, vec!["hello world", "foo bar"]);
835:     }
836: 
837:     #[test]
838:     fn exec_command_line_no_args() {
839:         let cmd = ExecCommand::parse("/usr/bin/daemon");
840:         assert_eq!(cmd.command_line(), "/usr/bin/daemon");
841:     }
842: 
843:     #[test]
844:     fn exec_command_line_with_args() {
845:         let cmd = ExecCommand::parse("-/usr/sbin/sshd -D -f /etc/ssh/sshd_config");
846:         assert_eq!(cmd.command_line(), "/usr/sbin/sshd -D -f /etc/ssh/sshd_config");
847:     }
848: 
849:     #[test]
850:     fn shell_words_quoted() {
851:         let words = shell_words("foo 'bar baz' \"qux quux\"");
852:         assert_eq!(words, vec!["foo", "bar baz", "qux quux"]);
853:     }
854: 
855:     #[test]
856:     fn shell_words_escaped() {
857:         let words = shell_words(r"foo\ bar baz");
858:         assert_eq!(words, vec!["foo bar", "baz"]);
859:     }
860: }

(End of file - total 860 lines)
</content>
```

**Tool: read**

**Input:**
```json
{
  "filePath": "/home/chenpi11/Projects/systema/systema/docs/allocator-todo.md"
}
```

**Output:**
```
<path>/home/chenpi11/Projects/systema/systema/docs/allocator-todo.md</path>
<type>file</type>
<content>
1: # System Allocator (System A) 功能实现 TODO 列表
2: 
3: > 根据软件工程设计图与问题陈述整理，对照当前 `crates/system-a` 代码的实际实现状态。
4: >
5: > **状态说明：**
6: > - ✅ **完全实现**：功能与设计目标完全一致
7: > - ⚠️ **部分实现**：已有代码，但功能不完整或存在偏差（附说明）
8: > - ❌ **未实现**：功能尚不存在
9: 
10: ---
11: 
12: ## 一、Unit 加载器（Unit Loader）
13: 
14: **实现文件：** `crates/system-a/src/unit/`
15: 
16: ### 1.1 Unit 文件解析
17: 
18: | 功能 | 状态 | 说明 |
19: |------|------|------|
20: | INI 格式解析（大小写不敏感的 section/key） | ✅ 完全实现 | 基于 `configparser` |
21: | `[Unit]` 节解析（Description, After, Before, Requires, Wants, Conflicts, PartOf, BindsTo） | ✅ 完全实现 | |
22: | `[Install]` 节解析（WantedBy, RequiredBy, Also, Alias） | ✅ 完全实现 | |
23: | `[Service]` 节解析（ExecStart, ExecStop, ExecReload, Type, Restart, User, Group, WorkingDirectory 等） | ✅ 完全实现 | 支持主要字段，包括 WatchdogUSec |
24: | `.service` 单元类型识别 | ✅ 完全实现 | |
25: | `.target` 单元类型识别 | ✅ 完全实现 | |
26: | `.mount` 单元类型识别（数据结构） | ✅ 完全实现 | MountSection 已定义并解析 |
27: | `.timer` 单元类型识别（数据结构） | ✅ 完全实现 | TimerSection 已定义并解析 |
28: | `.socket` 单元类型识别（数据结构） | ✅ 完全实现 | SocketSection 已定义并解析 |
29: | `.swap` / `.path` / `.slice` / `.scope` / `.device` | ✅ 完全实现 | 所有类型已定义并解析 |
30: | Drop-in 覆盖文件支持（`xxx.service.d/*.conf`） | ✅ 完全实现 | 支持所有单元类型的 Drop-in 覆盖 |
31: | `%n` / `%p` / `%i` 等 specifier 展开 | ✅ 完全实现 | ExecStart 中的 % 变量已被展开 |
32: | 条件检查（`ConditionPathExists=`, `ConditionHost=` 等） | ✅ 完全实现 | 启动时执行检查，不满足则跳过 |
33: | 断言检查（`AssertPathExists=` 等） | ✅ 完全实现 | 启动时执行检查，不满足则标记失败 |
34: | 多行值（续行符 `\`） | ✅ 完全实现 | |
35: | `ExecStart=-/cmd`（前缀 `-` 允许失败） | ✅ 完全实现 | ExecCommand 解析并记录 ignore_failure |
36: | `ExecStart=+/cmd`（前缀 `+` 赋予 root 权限） | ✅ 完全实现 | ExecCommand 解析并记录 privileged |
37: | `@`、`:`、`!`、`!!` 命令前缀处理 | ✅ 完全实现 | ExecCommand 解析所有前缀标志 |
38: 
39: ### 1.2 Unit 文件搜索与加载
40: 
41: | 功能 | 状态 | 说明 |
42: |------|------|------|
43: | 搜索路径列表（7个标准路径） | ✅ 完全实现 | `/etc/systemd/system`、`/run/systemd/system` 等 |
44: | 按需加载（`GetUnit`/`LoadUnit` 触发） | ✅ 完全实现 | |
45: | 启动时批量扫描加载所有单元文件 | ✅ 完全实现 | 递归扫描所有目录和子目录 |
46: | 单元文件变更监控（inotify）并自动重新加载 | 🚫 不计划实现 | 使用 inotify 监控文件变化并自动重载 |
47: | 单元生成器（Generator）支持（`/lib/systemd/system-generators/`） | ✅ 完全实现 | 支持标准 systemd 生成器目录 |
48: | 临时单元（Transient Unit）注册 | ✅ 完全实现 | `register_transient_unit` 函数支持运行时注册 |
49: | 单元卸载（从内存中移除不再需要的单元） | ✅ 完全实现 | `unload_unit` 函数支持卸载 |
50: | 单元别名（Alias）解析 | ✅ 完全实现 | `resolve_alias` 和 `get_unit_aliases` 函数支持 |
51: | 单元遮蔽（Mask，链接到 `/dev/null`）识别 | ✅ 完全实现 | `is_unit_masked` 函数检测遮蔽单元 |
52: 
53: ---
54: 
55: ## 二、依赖图构建（Dependency Graph）
56: 
57: **实现文件：** `crates/system-a/src/graph/mod.rs`
58: 
59: | 功能 | 状态 | 说明 |
60: |------|------|------|
61: | 基于 petgraph 的有向无环图（DAG）构建 | ✅ 完全实现 | |
62: | `After=` / `Before=` 排序依赖边 | ✅ 完全实现 | |
63: | `Requires=` / `Wants=` 语义记录 | ✅ 完全实现 | 记录在 `UnitSection` 中 |
64: | 未知单元的占位节点 | ✅ 完全实现 | |
65: | 拓扑排序（检测循环依赖） | ✅ 完全实现 | |
66: | `Requisite=` 依赖处理（前置依赖，必须已激活） | ✅ 完全实现 | 字段已建模、解析，调度时执行激活状态检查 |
67: | `BindsTo=` 依赖的实际生命周期绑定 | ✅ 完全实现 | 绑定单元停止/失败时自动停止依赖者 |
68: | `PartOf=` 的停止传播 | ✅ 完全实现 | 父单元停止时自动停止 PartOf 关联单元 |
69: | `Upholds=` 持续维持激活 | ✅ 完全实现 | 被维持单元停止/失败时自动重启 |
70: | `Conflicts=` 冲突处理（启动时停止冲突单元） | ✅ 完全实现 | 启动前自动停止冲突单元 |
71: | `OnSuccess=` / `OnFailure=` 触发 | ✅ 完全实现 | 任务成功/失败后触发指定单元启动 |
72: | `PropagatesReloadTo=` 重载传播 | ✅ 完全实现 | 重载时传播到声明的目标单元 |
73: | 反向依赖的停止传播（停止 A 时停止所有依赖 A 的 B） | ✅ 完全实现 | 停止时级联停止 Requires/BindsTo/PartOf 依赖者 |
74: | 循环依赖自动打破（systemd 的 cycle-breaking 机制） | ✅ 完全实现 | 优先移除弱边（Wants/After/Before），保留强边 |
75: 
76: ---
77: 
78: ## 三、拓扑排序与任务生成（Task Generator）
79: 
80: **实现文件：** `crates/system-a/src/scheduler/mod.rs`
81: 
82: | 功能 | 状态 | 说明 |
83: |------|------|------|
84: | 启动：拓扑排序生成 `WorkerTask` 列表 | ✅ 完全实现 | BFS + DFS 展开依赖，按 After/Before 排序 |
85: | 停止：生成 Stop 任务派发给 Worker | ✅ 完全实现 | 包含传播停止到下游依赖单元 |
86: | 重启：生成 Restart 任务 | ✅ 完全实现 | |
87: | 重载：生成 Reload 任务 | ✅ 完全实现 | |
88: | Target 单元内部激活（无需外部 Worker） | ✅ 完全实现 | |
89: | `task_id → JobKind` 映射（正确更新 ActiveState） | ✅ 完全实现 | |
90: | 任务结果处理（`handle_task_result`） | ✅ 完全实现 | 含 BindsTo/PartOf/OnSuccess/OnFailure/Upholds 后处理 |
91: | Job 模式（mode 参数：`replace`/`fail`/`queue`/`isolate`/`flush`/`ignore-dependencies`/`ignore-requirements`） | ✅ 完全实现 | `JobMode` 枚举+`from_str`解析，D-Bus 入口传递 mode |
92: | 作业冲突检测（已有同类作业时的行为） | ✅ 完全实现 | `fail`→报错，`queue`→返回已有job id，`replace`→取消旧job |
93: | `isolate` 模式（启动目标并停止所有其他单元） | ✅ 完全实现 | `handle_isolate` 遍历 runtime 停止非依赖单元 |
94: | `ignore-dependencies` 模式 | ✅ 完全实现 | 仅处理被请求单元，跳过依赖展开 |
95: | `ignore-requirements` 模式 | ✅ 完全实现 | 仅处理被请求单元，跳过依赖展开和 Requisite 检查 |
96: | 停止时传播：停止依赖下游单元 | ✅ 完全实现 | `compute_stop_order` 通过 Requires/BindsTo/PartOf 传播 |
97: | 串行任务执行（等待前一个完成再派下一个） | ✅ 完全实现 | `serial_completion_txs` + oneshot 通道链，`handle_task_result` 触发下一任务 |
98: | 启动超时监控（`TimeoutStartSec=`） | ✅ 完全实现 | `tokio::spawn` 延迟任务，超时后标记 Failed 并发出 Timeout 信号 |
99: | 作业运行超时（`JobRunningTimeoutUSec=`） | ✅ 完全实现 | 超时监控使用两倍 TimeoutStartSec 作为默认值 |
100: | 事件总线：`process.exit` 触发自动重启 | ✅ 完全实现 | 检查 RestartPolicy，按策略自动重启并应用限流 |
101: | 重启策略执行（`Restart=on-failure/always` 等） | ✅ 完全实现 | `handle_task_result` 和 `handle_event` 中检查策略并触发重启 |
102: | 重启限流（`StartLimitIntervalSec=`/`StartLimitBurst=`） | ✅ 完全实现 | `StartLimitState` 按时间窗口+突发次数限制 |
103: | `D-Bus JobNew / JobRemoved` 信号发出 | ✅ 完全实现 | `job_new_tx` 通道 + `dbus/mod.rs` 中异步发射 `JobNew` |
104: 
105: ---
106: 
107: ## 四、期望状态管理（Desired State Manager）
108: 
109: **实现文件：** `crates/system-a/src/state.rs`
110: 
111: | 功能 | 状态 | 说明 |
112: |------|------|------|
113: | `desired: HashMap<String, DesiredState>` 期望状态存储 | ✅ 完全实现 | |
114: | `runtime: HashMap<String, UnitRuntimeInfo>` 运行时状态存储 | ✅ 完全实现 | |
115: | `ActiveState` 枚举（Inactive/Activating/Active/Deactivating/Failed/Reloading） | ✅ 完全实现 | |
116: | `JobKind` 枚举（Start/Stop/Restart/Reload） | ✅ 完全实现 | |
117: | `JobStatus` 枚举（Waiting/Running/Done/Failed/Cancelled） | ✅ 完全实现 | |
118: | `JobResultKind` 枚举（Done/Failed/Cancelled/Timeout/Dependency/Skipped） | ✅ 完全实现 | |
119: | Job 完成通知（one-shot channel `completion_tx`） | ⚠️ 部分实现 | 字段已定义，但 D-Bus 层不使用（调用者不等待 job 完成） |
120: | Worker 注册表（`workers: HashMap`） | ✅ 完全实现 | |
121: | `task_kinds: HashMap<u64, JobKind>` task 与 job 映射 | ✅ 完全实现 | |
122: | 系统崩溃后恢复：与 Worker 重新同步状态 | ❌ 未实现 | System A 重启后 runtime 状态清空，Worker 状态不同步 |
123: | 单元引用计数（`RefUnit`/`UnrefUnit`） | ❌ 未实现 | |
124: | 期望状态与实际状态对账（reconciliation loop） | ❌ 未实现 | 无持续对账机制 |
125: | `InvocationID`（每次启动的唯一 UUID） | ❌ 未实现 | |
126: 
127: ---
128: 
129: ## 五、IPC 服务器（内部通信）
130: 
131: **实现文件：** `crates/system-a/src/ipc/server.rs`
132: 
133: | 功能 | 状态 | 说明 |
134: |------|------|------|
135: | Unix 域套接字服务器监听 `/run/systema/allocator.sock` | ✅ 完全实现 | |
136: | 长度前缀帧编解码（LengthDelimitedCodec） | ✅ 完全实现 | |
137: | `Envelope` protobuf 封装 | ✅ 完全实现 | |
138: | Worker 注册握手（`worker.register` → `worker.ack`） | ✅ 完全实现 | |
139: | 全双工连接（读写 split） | ✅ 完全实现 | |
140: | 任务派发（`task.dispatch`） | ✅ 完全实现 | |
141: | 任务结果接收（`task.result`） | ✅ 完全实现 | |
142: | 事件接收（`event.publish`：`process.exit`、`service.started`、`service.failed`） | ✅ 完全实现 | |
143: | Worker 断线检测与清理 | ✅ 完全实现 | |
144: | Worker 断线后自动重新连接（System A 视角） | ❌ 未实现 | Worker 断线后仅注销，不尝试重新连接 |
145: | 多个同类型 Worker 负载均衡 | ❌ 未实现 | 仅取第一个匹配的 Worker |
146: | 事件总线 Pub/Sub 机制（任意订阅者） | ❌ 未实现 | 当前仅 System A 处理事件，无通用发布订阅 |
147: | `SOCK_SEQPACKET` 传输层（当前为 `SOCK_STREAM`） | ❌ 未实现 | 当前使用 `UnixListener`（SOCK_STREAM）而非设计中的 SEQPACKET |
148: | 反压控制（背压机制） | ❌ 未实现 | 任务通道满时直接丢弃 |
149: 
150: ---
151: 
152: ## 六、D-Bus 服务器（外部兼容层）
153: 
154: **实现文件：** `crates/system-a/src/dbus/`
155: 
156: | 功能 | 状态 | 说明 |
157: |------|------|------|
158: | 声明 `org.freedesktop.systemd1` 总线名称 | ✅ 完全实现 | |
159: | `org.freedesktop.systemd1.Manager` 接口注册（`/org/freedesktop/systemd1`） | ✅ 完全实现 | |
160: | 核心管理方法（Start/Stop/Restart/Reload/ListUnits 等） | ⚠️ 部分实现 | 详见 `dbus-todo.md` |
161: | 管理器属性（Version/Architecture/NNames 等） | ⚠️ 部分实现 | 大量属性为固定值，详见 `dbus-todo.md` |
162: | 每单元 D-Bus 对象动态注册（`Unit`/`Service`/`Target` 接口） | ⚠️ 部分实现 | 已动态注册基础 `Unit` 与 `Service`；`Target` 专有接口尚未注册 |
163: | 每作业 D-Bus 对象动态注册（`Job` 接口） | ❌ 未实现 | 管理方法会返回 `job/<id>` 路径，但该路径未注册 `Job` 对象 |
164: | D-Bus 信号（`UnitNew`/`UnitRemoved`/`JobNew`/`JobRemoved`/`StartupFinished`） | ⚠️ 部分实现 | 已发出 `JobRemoved`，其余信号未实现 |
165: | polkit 权限检查 | ❌ 未实现 | 所有方法无访问控制 |
166: | `org.freedesktop.DBus.Properties` 标准接口 | ⚠️ 部分实现 | zbus 自动为 Manager 和已注册 Unit/Service 对象提供 |
167: | `org.freedesktop.DBus.Introspectable` 标准接口 | ⚠️ 部分实现 | zbus 自动为 Manager 生成 |
168: | `org.freedesktop.DBus.ObjectManager` 接口 | ❌ 未实现 | 无法枚举所有 D-Bus 对象 |
169: | `PropertiesChanged` 信号（属性变更通知） | ❌ 未实现 | |
170: 
171: ---
172: 
173: ## 七、并发模型与整体架构
174: 
175: | 功能 | 状态 | 说明 |
176: |------|------|------|
177: | 单线程异步事件循环（`tokio::main(flavor = "current_thread")`） | ✅ 完全实现 | |
178: | IPC 服务器与 D-Bus 服务器并发运行（`tokio::select!`） | ✅ 完全实现 | |
179: | `AllocatorState` 通过 `parking_lot::RwLock` 保护 | ✅ 完全实现 | |
180: | 锁不跨 `.await` 持有（避免死锁） | ✅ 完全实现 | |
181: | 严格状态分离（System A 不持有真实 PID 或进程状态） | ✅ 完全实现 | |
182: | System A 崩溃重启后恢复（重读单元文件并同步 Worker 状态） | ❌ 未实现 | Phase 2 功能 |
183: | 日志记录（tracing 框架） | ✅ 完全实现 | |
184: | 结构化错误处理（anyhow） | ✅ 完全实现 | |
185: 
186: ---
187: 
188: ## 八、其他 Worker 组件（System A 视角的依赖）
189: 
190: System A 与以下 Worker 通过 IPC 交互，以下是 System A 侧对各 Worker 的支持状态：
191: 
192: | Worker | 状态 | 说明 |
193: |--------|------|------|
194: | **System S**（Service Worker） | ⚠️ 部分实现 | IPC 通信已实现；System S 已实现基础状态机和进程管理；重启策略、cgroup、看门狗等未实现 |
195: | **System T**（Target Worker） | ⚠️ 部分实现 | Target 内部激活逻辑已内嵌于 System A，无独立 Worker；但不能响应来自 Target 的事件 |
196: | **System M**（Mount Worker） | ❌ 未实现 | `crates/system-m` 不存在 |
197: | **System C**（Cron/Timer Worker） | ❌ 未实现 | `crates/system-c` 不存在 |
198: | **System P**（Power Worker） | ⚠️ 部分实现 | `crates/backend/systema-sysp`（`.linux` 用 libc `reboot(2)`，`.shim` 为 no-op）；SysI 自动拉起：Linux 用 `systema-sysp.linux`，其他平台用 `systema-sysp.shim`；休眠/睡眠仍待与 System C/K 编排 |
199: 
200: ---
201: 
202: ## 九、平台抽象
203: 
204: | 功能 | 状态 | 说明 |
205: |------|------|------|
206: | Linux 平台（主要目标） | ⚠️ 部分实现 | System A 本身无平台相关代码；System S 使用 tokio::process（跨平台），但 SIGTERM 使用 nix（Linux 专用） |
207: | 统一 `ProcessHandle` 接口 | ❌ 未实现 | 尚无平台抽象层 |
208: | Windows 平台支持 | ❌ 未实现 | Phase 4 功能 |
209: 
210: ---
211: 
212: ## 十、测试覆盖
213: 
214: | 测试项 | 状态 | 说明 |
215: |--------|------|------|
216: | Unit 文件解析单元测试（`.service`） | ✅ 完全实现 | |
217: | Unit 文件解析单元测试（`.target`） | ✅ 完全实现 | |
218: | 依赖图拓扑排序测试 | ✅ 完全实现 | |
219: | 任务调度器测试（JobMode、start/stop order、条件检查、限流等） | ✅ 完全实现 | 34 个单元测试覆盖所有调度功能 |
220: | IPC 客户端-服务器集成测试 | ❌ 未实现 | |
221: | D-Bus 接口集成测试 | ❌ 未实现 | |
222: | 端到端测试（`systemctl start/stop`） | ❌ 未实现 | |

(End of file - total 222 lines)
</content>
```

---

## Assistant (Plan · DeepSeek V4 Flash Free · 24.7s)

_Thinking:_

Now I have a complete understanding. Let me formulate the Phase 2 plan.

Phase 2 requirements from docs/prompt.md:
1. **事件总线：pub/sub 机制**，System S 发布 `process.exit`，SysA 触发 restart。
2. **重启策略（on-failure, always）**。
3. **System A 能正确从崩溃中恢复**：重读 Unit 文件，与各 System E 恢复连接并同步状态。

What already exists:
- System S already publishes events (service.started, service.failed, process.exit)
- System A already handles these events and triggers restarts based on RestartPolicy
- Restart policy (on-failure, always, etc.) is already partially implemented in both `handle_event` and `handle_task_result`

What's actually missing for Phase 2 (based on thorough analysis):

### 1. 事件总线 pub/sub 机制
- Currently, events flow only from Worker → System A, with no general pub/sub
- Need a proper event bus with topics, subscriber registration, and broadcast
- This would allow future components (like a logging subsystem, or external monitoring) to subscribe to events

### 2. 重启策略完善
- The restart policy in `handle_event` uses hardcoded rate limit values (interval=10s, burst=5) instead of `StartLimitIntervalSec`/`StartLimitBurst` from unit file
- The restart policy implementation could benefit from being unified (currently duplicated between `handle_event` and `handle_task_result`)
- Need to read `StartLimitIntervalSec` and `StartLimitBurst` from the unit file parser
- Missing `RestartSteps` and `RestartMaxDelaySec` support

### 3. System A 崩溃恢复
- Completely missing
- Need:
  a. State persistence (save desired state and job state to disk)
  b. On restart: reload unit files, reload persistent state
  c. Re-establish connections with workers
  d. State reconciliation: query each worker for its current state, reconcile with desired state
  e. If desired state says a unit should be active but worker says it's dead, re-issue start task

Let me create a detailed, actionable plan.

现在我已完全理解代码库状态。以下是 Phase 2 的详细计划。

---

## Phase 2 计划：事件总线、重启策略完善与崩溃恢复

根据 `docs/prompt.md` Phase 2 要求，结合代码库现有状态，分为以下三大模块：

---

### 模块 A：事件总线 Pub/Sub 机制

**现状：** 事件目前仅从 Worker → System A 单向流动，无通用订阅机制。

**需要实现：**

1. **`EventBus` 核心类型**（新增 `crates/common/src/event_bus.rs`）
   - `EventTopic` 枚举/字符串 — `ProcessExit`, `ServiceStarted`, `ServiceFailed`, `UnitStateChange` 等
   - `EventSubscriber` trait — 接收 `(topic, unit_name, data)` 回调
   - `EventBus` 结构体 — 持有 `HashMap<EventTopic, Vec<Box<dyn EventSubscriber>>>`
   - 线程安全（`Arc<RwLock<EventBus>>`）

2. **System A 内部集成**
   - 在 `AllocatorState` 中增加 `event_bus: EventBus` 字段
   - 将 `handle_event()` 中的直接状态更新改为通过 `EventBus` 派发
   - 让 System A 自己的 event handler 也作为订阅者注册到 EventBus
   - 允许 D-Bus 层订阅 `UnitStateChange` 以发送 `PropertiesChanged` 信号

3. **对外暴露**（可选，视复杂度）
   - 可新增 IPC 方法 `event.subscribe`/`event.unsubscribe`，允许其他组件订阅事件流

**改动文件：**
| 文件 | 改动 |
|------|------|
| `crates/common/src/lib.rs` | 新增 `pub mod event_bus;` |
| `crates/common/src/event_bus.rs` | **新建** — `EventBus`, `EventTopic`, `EventSubscriber` |
| `crates/system-a/src/state.rs` | `AllocatorState` 添加 `event_bus` 字段 |
| `crates/system-a/src/ipc/server.rs` | `handle_event()` 改为走 EventBus 分发 |
| `crates/system-a/src/main.rs` | 初始化时创建 EventBus 并注册 System A 自身的 handler |

---

### 模块 B：重启策略完善

**现状：** 重启策略已部分实现，但有如下问题：
- 两个地方实现重启逻辑（`handle_event` 和 `handle_task_result`），逻辑重复
- 限流参数硬编码（interval=10s, burst=5），未从 unit 文件读取
- `StartLimitIntervalSec`/`StartLimitBurst` 尚未在 `ServiceSection` 中解析
- `RestartSteps`/`RestartMaxDelaySec` 未支持
- 缺少 systemd 精确的 `Restart=` 行为分类表

**需要实现：**

1. **Unit 文件解析增强** — 在 `ServiceSection` 中增加：
   - `StartLimitIntervalSec` / `StartLimitBurst`（已有 `StartLimitState` 但未从 unit 文件读取）
   - `StartLimitAction`（none, reboot, reboot-force, reboot-immediate, poweroff, exit）
   - `RestartSteps` / `RestartMaxDelaySec`

2. **重启逻辑统一与移动至 `scheduler/mod.rs`**
   - 提取 `handle_event` 中 `process.exit` 的重启逻辑
   - 统一 `handle_event` 和 `handle_task_result` 中重启判断逻辑
   - 改为调用公共函数 `should_restart_service()` / `schedule_restart()`
   - 从 `UnitFile` 读取 `StartLimitIntervalSec`/`StartLimitBurst` 替代硬编码

3. **精确的 Restart= 策略对照表**
   systemd 的 Restart= 策略是：
   | 策略 | 零退出码 | 非零退出码 | 异常信号 | 超时 | 看门狗 |
   |------|---------|-----------|---------|-----|-------|
   | no | ✗ | ✗ | ✗ | ✗ | ✗ |
   | on-success | ✓ | ✗ | ✗ | ✗ | ✗ |
   | on-failure | ✗ | ✓ | ✓ | ✓ | ✗ |
   | on-abnormal | ✗ | ✗ | ✓ | ✓ | ✗ |
   | on-watchdog | ✗ | ✗ | ✗ | ✗ | ✓ |
   | on-abort | ✗ | ✗ | ✓ | ✗ | ✗ |
   | always | ✓ | ✓ | ✓ | ✓ | ✓ |
   当前实现与上表存在偏差，需要修正。

**改动文件：**
| 文件 | 改动 |
|------|------|
| `crates/system-a/src/unit/types.rs` | `ServiceSection` 增加 `StartLimitIntervalSec`, `StartLimitBurst`, `StartLimitAction`, `RestartSteps`, `RestartMaxDelaySec` |
| `crates/system-a/src/unit/parser.rs` | 解析上述新字段 |
| `crates/system-a/src/scheduler/mod.rs` | 新增 `should_restart_service()` 函数；统一重启逻辑 |
| `crates/system-a/src/ipc/server.rs` | `handle_event` 中重启逻辑改为调用 `scheduler` 公共函数 |

---

### 模块 C：System A 崩溃恢复

**现状：** 完全未实现。

**需要实现：**

1. **状态持久化存储**
   - 在 `crates/common/src/` 新增 `persist.rs` 
   - `DesiredStateFile` — 以 JSON 格式序列化到 `/run/systema/state.json`
   - 每次 `desired` 状态变更时写盘（异步、防频繁写：去抖 500ms）
   - 持久化内容：`desired: HashMap<String, DesiredState>` + `last_state_version`
   - 设计考量：状态文件应在 crash 后仍然可读；使用原子写（写临时文件 → rename）

2. **启动恢复流程**（`system-a/src/main.rs` 及新 `recovery.rs`）
   启动顺序改为：
   ```
   1. 创建 AllocatorState（空状态）
   2. 加载 unit 文件（已有）
   3. 读取持久化状态文件，恢复 desired state
   4. 启动 IPC 服务器
   5. 等待 Worker 连接 → 每个 Worker 连接后：
      a. 正常注册握手
      b. System A 发送 `state.sync` IPC 请求
      c. Worker 回复当前每个 unit 的 `(unit_name, active_state, pid)`
      d. System A 将 Worker 报告的实际状态与持久化的 desired state 对比
      e. 不一致的：自动 enqueue 修复 job（Start desired-active 但实际不活的）
   6. 启动 D-Bus 服务器
   ```

3. **新增 IPC 方法**
   - `state.sync_request` (A→W)：请求 Worker 上报其管理的所有 unit 状态
   - `state.sync_report` (W→A)：Worker 回复 `[(unit_name, state, pid, exit_code)]`
   - 在 `proto/ipc.proto` 中新增 `StateSyncRequest` / `StateSyncReport` 消息

4. **System S 侧同步响应**
   - `system-s/src/ipc/mod.rs` 新增对 `state.sync_request` 的处理
   - 遍历 `ServiceRegistry`，返回每个 service 的当前 `ServiceState`、`main_pid`、`last_exit_code`

5. **Reconciliation Loop**（持续对账）
   - 考虑新增可选的对账循环：周期（如 30s）ping Worker 检查存活状态
   - 简单的实现：利用已有的 `process.exit` 事件即可，无需心跳
   - 完整实现可考虑心跳 + 状态对账

6. **Worker 端重连增强**
   - System S 已有重连逻辑（指数退避）
   - 断线重连后，System A 应重新发送 `state.sync_request` 以补齐状态

**改动文件：**
| 文件 | 改动 |
|------|------|
| `proto/ipc.proto` | 新增 `StateSyncRequest`, `StateSyncReport`, `SyncUnitState` 消息 |
| `crates/common/src/lib.rs` | 新增 `pub mod persist;` |
| `crates/common/src/persist.rs` | **新建** — 状态序列化/反序列化函数 |
| `crates/system-a/src/recovery.rs` | **新建** — 启动恢复逻辑 |
| `crates/system-a/src/main.rs` | 集成恢复流程 |
| `crates/system-a/src/state.rs` | 增加 `desired_state_version`, `persist_dirty` 标志 |
| `crates/system-a/src/ipc/server.rs` | 处理 `state.sync_request`/`state.sync_report`；Worker 重连后触发同步 |
| `crates/system-a/src/scheduler/mod.rs` | `handle_task_result` 成功后触发 persist |
| `crates/system-s/src/ipc/mod.rs` | 处理 `state.sync_request`，回复 `state.sync_report` |
| `crates/common/build.rs` | 重新生成 proto（`state_sync_request`/`state_sync_report` 可能需要导出 `paths.rs`，但已有） |

---

### 实施顺序建议

```
Phase 2 实施顺序:
1. 模块 B（重启策略完善）— 改动最小，单元测试覆盖最易
   - 解析 StartLimitIntervalSec/StartLimitBurst
   - 统一重启逻辑，修正 Restart= 策略表
2. 模块 A（事件总线）— 架构增强
   - 实现 EventBus，重构 handle_event
3. 模块 C（崩溃恢复）— 依赖模块 A/B，改动最大
   - 状态持久化
   - IPC 新增同步方法
   - Worker 侧响应同步
   - 启动恢复流程
   - Reconciliation loop
```

---

需要我调整计划细节或开始实现某个模块吗？

---

