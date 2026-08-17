# System Allocator (System A) 功能实现 TODO 列表

> 根据软件工程设计图与问题陈述整理，对照当前 `crates/backend/systema-sysa` 代码的实际实现状态。
> （最后核对：2026-08-16，M1/M2 完成后）
>
> **状态说明：**
> - ✅ **完全实现**：功能与设计目标完全一致
> - ⚠️ **部分实现**：已有代码，但功能不完整或存在偏差（附说明）
> - ❌ **未实现**：功能尚不存在

---

## 一、Unit 加载器（Unit Loader）

**实现文件：** `crates/backend/systema-sysa/src/unit/`

### 1.1 Unit 文件解析

| 功能 | 状态 | 说明 |
|------|------|------|
| INI 格式解析（大小写不敏感的 section/key） | ✅ 完全实现 | 基于 `configparser` |
| `[Unit]` 节解析（Description, After, Before, Requires, Wants, Conflicts, PartOf, BindsTo） | ✅ 完全实现 | |
| `[Install]` 节解析（WantedBy, RequiredBy, Also, Alias） | ✅ 完全实现 | |
| `[Service]` 节解析（ExecStart, ExecStop, ExecReload, Type, Restart, User, Group, WorkingDirectory 等） | ✅ 完全实现 | 支持主要字段，包括 WatchdogUSec |
| `.service` 单元类型识别 | ✅ 完全实现 | |
| `.target` 单元类型识别 | ✅ 完全实现 | |
| `.mount` 单元类型识别（数据结构） | ✅ 完全实现 | MountSection 已定义并解析 |
| `.timer` 单元类型识别（数据结构） | ✅ 完全实现 | TimerSection 已定义并解析 |
| `.socket` 单元类型识别（数据结构） | ✅ 完全实现 | SocketSection 已定义并解析 |
| `.swap` / `.path` / `.slice` / `.scope` / `.device` | ✅ 完全实现 | 所有类型已定义并解析 |
| Drop-in 覆盖文件支持（`xxx.service.d/*.conf`） | ✅ 完全实现 | 支持所有单元类型的 Drop-in 覆盖 |
| `%n` / `%p` / `%i` 等 specifier 展开 | ✅ 完全实现 | ExecStart 中的 % 变量已被展开 |
| 条件检查（`ConditionPathExists=`, `ConditionHost=` 等） | ✅ 完全实现 | 启动时执行检查，不满足则跳过 |
| 断言检查（`AssertPathExists=` 等） | ✅ 完全实现 | 启动时执行检查，不满足则标记失败 |
| 多行值（续行符 `\`） | ✅ 完全实现 | |
| `ExecStart=-/cmd`（前缀 `-` 允许失败） | ✅ 完全实现 | ExecCommand 解析并记录 ignore_failure |
| `ExecStart=+/cmd`（前缀 `+` 赋予 root 权限） | ✅ 完全实现 | ExecCommand 解析并记录 privileged |
| `@`、`:`、`!`、`!!` 命令前缀处理 | ✅ 完全实现 | ExecCommand 解析所有前缀标志 |

### 1.2 Unit 文件搜索与加载

| 功能 | 状态 | 说明 |
|------|------|------|
| 搜索路径列表（7个标准路径） | ✅ 完全实现 | `/etc/systemd/system`、`/run/systemd/system` 等 |
| 按需加载（`GetUnit`/`LoadUnit` 触发） | ✅ 完全实现 | |
| 启动时批量扫描加载所有单元文件 | ✅ 完全实现 | 递归扫描所有目录和子目录 |
| 单元文件变更监控（inotify）并自动重新加载 | 🚫 不计划实现 | 使用 inotify 监控文件变化并自动重载 |
| 单元生成器（Generator）支持（`/lib/systemd/system-generators/`） | ✅ 完全实现 | 支持标准 systemd 生成器目录 |
| 临时单元（Transient Unit）注册 | ✅ 完全实现 | `register_transient_unit` 函数支持运行时注册 |
| 单元卸载（从内存中移除不再需要的单元） | ✅ 完全实现 | `unload_unit` 函数支持卸载 |
| 单元别名（Alias）解析 | ✅ 完全实现 | `resolve_alias` 和 `get_unit_aliases` 函数支持 |
| 单元遮蔽（Mask，链接到 `/dev/null`）识别 | ✅ 完全实现 | `is_unit_masked` 函数检测遮蔽单元 |

---

## 二、依赖图构建（Dependency Graph）

**实现文件：** `crates/backend/systema-sysa/src/graph/mod.rs`

| 功能 | 状态 | 说明 |
|------|------|------|
| 基于 petgraph 的有向无环图（DAG）构建 | ✅ 完全实现 | |
| `After=` / `Before=` 排序依赖边 | ✅ 完全实现 | |
| `Requires=` / `Wants=` 语义记录 | ✅ 完全实现 | 记录在 `UnitSection` 中 |
| 未知单元的占位节点 | ✅ 完全实现 | |
| 拓扑排序（检测循环依赖） | ✅ 完全实现 | |
| `Requisite=` 依赖处理（前置依赖，必须已激活） | ✅ 完全实现 | 字段已建模、解析，调度时执行激活状态检查 |
| `BindsTo=` 依赖的实际生命周期绑定 | ✅ 完全实现 | 绑定单元停止/失败时自动停止依赖者 |
| `PartOf=` 的停止传播 | ✅ 完全实现 | 父单元停止时自动停止 PartOf 关联单元 |
| `Upholds=` 持续维持激活 | ✅ 完全实现 | 被维持单元停止/失败时自动重启 |
| `Conflicts=` 冲突处理（启动时停止冲突单元） | ✅ 完全实现 | 启动前自动停止冲突单元 |
| `OnSuccess=` / `OnFailure=` 触发 | ✅ 完全实现 | 任务成功/失败后触发指定单元启动 |
| `PropagatesReloadTo=` 重载传播 | ✅ 完全实现 | 重载时传播到声明的目标单元 |
| 反向依赖的停止传播（停止 A 时停止所有依赖 A 的 B） | ✅ 完全实现 | 停止时级联停止 Requires/BindsTo/PartOf 依赖者 |
| 循环依赖自动打破（systemd 的 cycle-breaking 机制） | ✅ 完全实现 | 优先移除弱边（Wants/After/Before），保留强边 |

---

## 三、拓扑排序与任务生成（Task Generator）

**实现文件：** `crates/backend/systema-sysa/src/scheduler/mod.rs`

| 功能 | 状态 | 说明 |
|------|------|------|
| 启动：拓扑排序生成 `WorkerTask` 列表 | ✅ 完全实现 | BFS + DFS 展开依赖，按 After/Before 排序 |
| 停止：生成 Stop 任务派发给 Worker | ✅ 完全实现 | 包含传播停止到下游依赖单元 |
| 重启：生成 Restart 任务 | ✅ 完全实现 | |
| 重载：生成 Reload 任务 | ✅ 完全实现 | |
| Target 单元内部激活（无需外部 Worker） | ✅ 完全实现 | |
| `task_id → JobKind` 映射（正确更新 ActiveState） | ✅ 完全实现 | |
| 任务结果处理（`handle_task_result`） | ✅ 完全实现 | 含 BindsTo/PartOf/OnSuccess/OnFailure/Upholds 后处理 |
| Job 模式（mode 参数：`replace`/`fail`/`queue`/`isolate`/`flush`/`ignore-dependencies`/`ignore-requirements`） | ✅ 完全实现 | `JobMode` 枚举+`from_str`解析，D-Bus 入口传递 mode |
| 作业冲突检测（已有同类作业时的行为） | ✅ 完全实现 | `fail`→报错，`queue`→返回已有job id，`replace`→取消旧job |
| `isolate` 模式（启动目标并停止所有其他单元） | ✅ 完全实现 | `handle_isolate` 遍历 runtime 停止非依赖单元 |
| `ignore-dependencies` 模式 | ✅ 完全实现 | 仅处理被请求单元，跳过依赖展开 |
| `ignore-requirements` 模式 | ✅ 完全实现 | 仅处理被请求单元，跳过依赖展开和 Requisite 检查 |
| 停止时传播：停止依赖下游单元 | ✅ 完全实现 | `compute_stop_order` 通过 Requires/BindsTo/PartOf 传播 |
| 串行任务执行（等待前一个完成再派下一个） | ✅ 完全实现 | `serial_completion_txs` + oneshot 通道链，`handle_task_result` 触发下一任务 |
| 启动超时监控（`TimeoutStartSec=`） | ✅ 完全实现 | `tokio::spawn` 延迟任务，超时后标记 Failed 并发出 Timeout 信号 |
| 作业运行超时（`JobRunningTimeoutUSec=`） | ✅ 完全实现 | 超时监控使用两倍 TimeoutStartSec 作为默认值 |
| 事件总线：`process.exit` 触发自动重启 | ✅ 完全实现 | 检查 RestartPolicy，按策略自动重启并应用限流 |
| 重启策略执行（`Restart=on-failure/always` 等） | ✅ 完全实现 | `handle_task_result` 和 `handle_event` 中检查策略并触发重启 |
| 重启限流（`StartLimitIntervalSec=`/`StartLimitBurst=`） | ✅ 完全实现 | `StartLimitState` 按时间窗口+突发次数限制 |
| `D-Bus JobNew / JobRemoved` 信号发出 | ✅ 完全实现 | `job_new_tx` 通道 + `dbus/mod.rs` 中异步发射 `JobNew` |

---

## 四、期望状态管理（Desired State Manager）

**实现文件：** `crates/backend/systema-sysa/src/state.rs`

| 功能 | 状态 | 说明 |
|------|------|------|
| `desired: HashMap<String, DesiredState>` 期望状态存储 | ✅ 完全实现 | |
| `runtime: HashMap<String, UnitRuntimeInfo>` 运行时状态存储 | ✅ 完全实现 | |
| `ActiveState` 枚举（Inactive/Activating/Active/Deactivating/Failed/Reloading） | ✅ 完全实现 | |
| `JobKind` 枚举（Start/Stop/Restart/Reload） | ✅ 完全实现 | |
| `JobStatus` 枚举（Waiting/Running/Done/Failed/Cancelled） | ✅ 完全实现 | |
| `JobResultKind` 枚举（Done/Failed/Cancelled/Timeout/Dependency/Skipped） | ✅ 完全实现 | |
| Job 完成通知（one-shot channel `completion_tx`） | ✅ 完全实现 | 字段已定义，`handle_task_result` 通过 `completion_tx` 发送完成通知；D-Bus 层通过 `JobRemoved` 信号向调用者报告 |
| Worker 注册表（`workers: HashMap`） | ✅ 完全实现 | |
| `task_kinds: HashMap<u64, JobKind>` task 与 job 映射 | ✅ 完全实现 | |
| 系统崩溃后恢复：与 Worker 重新同步状态 | 🚫 不计划实现 | System A 重启后 runtime 状态清空，Worker 状态不同步 |
| 单元引用计数（`RefUnit`/`UnrefUnit`） | ✅ 完全实现 | `state.ref_counts` 记录单元间依赖引用；`ref_unit()`/`unref_unit()` 管理引用；`commit_staging()` 后自动重建；通过 D-Bus `refs` 属性暴露 |
| 期望状态与实际状态对账（reconciliation loop） | ✅ 完全实现 | `scheduler::start_reconciliation_loop()` 每 5 秒对比 `desired` vs `runtime`，自动启停不匹配的单元 |
| `InvocationID`（每次启动的唯一 UUID） | ✅ 完全实现 | 每次 Start/Restart 生成 UUIDv4；存入 `UnitRuntimeInfo.invocation_id`；通过 IPC 传递给 Worker 设为 `INVOCATION_ID=` 环境变量；通过 D-Bus `InvocationID` 属性暴露 |

---

## 五、IPC 服务器（内部通信）

**实现文件：** `crates/backend/systema-sysa/src/ipc/server.rs`

| 功能 | 状态 | 说明 |
|------|------|------|
| Unix 域套接字服务器监听 `/run/system-alphabet/allocator.sock` | ✅ 完全实现 | |
| 长度前缀帧编解码（LengthDelimitedCodec） | ✅ 完全实现 | |
| `Envelope` protobuf 封装 | ✅ 完全实现 | |
| Worker 注册握手（`worker.register` → `worker.ack`） | ✅ 完全实现 | |
| 全双工连接（读写 split） | ✅ 完全实现 | |
| 任务派发（`task.dispatch`） | ✅ 完全实现 | |
| 任务结果接收（`task.result`） | ✅ 完全实现 | |
| 事件接收（`event.publish`：`process.exit`、`service.started`、`service.failed`） | ✅ 完全实现 | |
| Worker 断线检测与清理 | ✅ 完全实现 | |
| Worker 断线后自动重新连接 | ✅ 完全实现 | 重连由 Worker 客户端侧实现（`libsysa/src/worker_ipc.rs:250`，退避重试）；System A 侧接受重新注册 |
| 多个同类型 Worker 负载均衡 | ❌ 未实现 | 仅取第一个匹配的 Worker |
| 事件总线 Pub/Sub 机制 | ✅ 完全实现 | `AllocatorState.event_bus`（`state.rs:338`，`EventBus`）；事件经 `dispatch` 分发给订阅者（`server.rs:1410`），用于 UnitStateChange 推送（D-Bus 信号 + WorkerEventForwarder 资源事件转发） |
| `SOCK_SEQPACKET` 传输层（当前为 `SOCK_STREAM`） | ❌ 未实现 | 当前使用 `UnixListener`（SOCK_STREAM）而非设计中的 SEQPACKET |
| 反压控制（背压机制） | ⚠️ 部分实现 | 每 worker 有界通道（容量 64，`server.rs:230`）；发送端 `await` 阻塞即天然背压（`scheduler/mod.rs:1144`），但无显式的降级/丢弃策略 |

---

## 六、D-Bus 服务器（外部兼容层）

**实现文件：** `crates/backend/systema-sysa/src/dbus/`

| 功能 | 状态 | 说明 |
|------|------|------|
| 声明 `org.freedesktop.systemd1` 总线名称 | ✅ 完全实现 | |
| `org.freedesktop.systemd1.Manager` 接口注册（`/org/freedesktop/systemd1`） | ✅ 完全实现 | |
| 核心管理方法（Start/Stop/Restart/Reload/ListUnits 等） | ⚠️ 部分实现 | 详见 `dbus-todo.md` |
| 管理器属性（Version/Architecture/NNames 等） | ⚠️ 部分实现 | 大量属性为固定值，详见 `dbus-todo.md` |
| 每单元 D-Bus 对象动态注册（`Unit`/`Service`/`Scope`/`Slice`/`Mount`/`Socket` 接口） | ⚠️ 部分实现 | 上述接口均已注册（`dbus/mod.rs` `register_unit_object`）；`Target`/`Timer`/`Path`/`Device`/`Swap`/`Automount` 专有接口未注册；详见 `dbus-todo.md` |
| 每作业 D-Bus 对象动态注册（`Job` 接口） | ❌ 未实现 | 管理方法会返回 `job/<id>` 路径，但该路径未注册 `Job` 对象 |
| D-Bus 信号（`UnitNew`/`UnitRemoved`/`JobNew`/`JobRemoved`/`StartupFinished`/`Reloading`） | ⚠️ 部分实现 | 已发出 `JobNew`/`JobRemoved`/`UnitRemoved`/`Reloading`；`UnitNew`/`StartupFinished`/`UnitFilesChanged` 未实现 |
| polkit 权限检查 | ❌ 未实现 | 所有方法无访问控制 |
| `org.freedesktop.DBus.Properties` 标准接口 | ⚠️ 部分实现 | 自定义实现（`dbus/properties.rs`）替换 zbus 默认分发：覆盖 Unit/Service/Socket/Mount/Slice 接口；**Scope 接口未处理**（`Get`/`GetAll(Scope)` 返回 `UnknownInterface`，见 `dbus-todo.md`） |
| `org.freedesktop.DBus.Introspectable` 标准接口 | ⚠️ 部分实现 | zbus 自动为 Manager 生成 |
| `org.freedesktop.DBus.ObjectManager` 接口 | ❌ 未实现 | 无法枚举所有 D-Bus 对象 |
| `PropertiesChanged` 信号（属性变更通知） | ❌ 未实现 | |

---

## 七、并发模型与整体架构

| 功能 | 状态 | 说明 |
|------|------|------|
| 单线程异步事件循环（`tokio::main(flavor = "current_thread")`） | ✅ 完全实现 | |
| IPC 服务器与 D-Bus 服务器并发运行（`tokio::select!`） | ✅ 完全实现 | |
| `AllocatorState` 通过 `parking_lot::RwLock` 保护 | ✅ 完全实现 | |
| 锁不跨 `.await` 持有（避免死锁） | ✅ 完全实现 | |
| 严格状态分离（System A 不持有真实 PID 或进程状态） | ✅ 完全实现 | |
| System A 崩溃重启后恢复（重读单元文件并同步 Worker 状态） | ❌ 未实现 | Phase 2 功能 |
| 日志记录（tracing 框架） | ✅ 完全实现 | |
| 结构化错误处理（anyhow） | ✅ 完全实现 | |

---

## 八、其他 Worker 组件（System A 视角的依赖）

System A 与以下 Worker 通过 IPC 交互，以下是 System A 侧对各 Worker 的支持状态：

| Worker | 状态 | 说明 |
|--------|------|------|
| **System S**（Service Worker） | ✅ 已实现 | `crates/backend/systema-syss`：服务状态机与进程管理（`tokio::process` 包装 fork/exec，`process.rs`）；ExecStart 解析（前缀、分词、% 说明符、$VAR 展开、`\|` shell 调用）；重启策略；SIGTERM 等信号用 nix（`Cargo.toml:27-28`）。未实现：命名空间/能力/seccomp 等执行上下文 |
| **System R**（Resource Worker） | ✅ 已实现 | `crates/backend/systema-sysr`：slice 单元 + cgroup v2 资源控制（memory/CPU/IO/tasks）；`unit.define` 合成 handler（`supports_unit_define`）；`cgroup.metrics` 推送 |
| **System E**（External Process Worker） | ✅ 已实现 | `crates/backend/systema-syse`：scope 单元（PIDs 包装、cgroup.events、RuntimeMaxSec、Abandon） |
| **System T**（Target Worker） | ✅ 已实现 | `crates/backend/systema-syst`：独立进程，target 状态跟踪（原内嵌于 System A） |
| **System C**（Cron/Timer Worker） | ✅ 已实现 | `crates/backend/systema-sysc`：timer 单元（单调 + 日历调度，`timer.fired`） |
| **System P**（Path Worker） | ✅ 已实现 | `crates/backend/systema-sysp`：path 单元（PathExists/Glob/Changed/Modified、DirectoryNotEmpty，inotify） |
| **System K**（Socket Worker） | ✅ 已实现 | `crates/backend/systema-sysk`：socket 单元（TCP/Unix 监听、fd 传递）；socket activation 仅 inetd 式（无 LISTEN_FDS/LISTEN_PID 环境、不关联配对 service） |
| **System D**（Device Worker） | ✅ 已实现 | `crates/backend/systema-sysd`：device 单元（/dev + sysfs 发现、netlink、match 规则） |
| **System M**（Mount Worker） | ✅ 已实现 | `crates/backend/systema-sysm`（+ `.linux` flavor）：mount / automount 单元 |
| **System F**（Finder Worker） | ⚠️ 部分实现 | `crates/backend/systema-sysf`：通用 finder worker（staging 区提交/查询）；`crates/backend/systema-sysf/systema-sysf-systemd`：systemd finder 可执行文件（`systema-sysf.systemd`，解析 + 暂存） |
| **System B**（Boot/Power Worker） | ❌ 未实现 | 无 `reboot/poweroff/halt/kexec` 等 D-Bus 方法；仅 `StartLimitAction=` 副作用调用外部 `shutdown` 命令 |
| **swap 单元** | ❌ 未实现 | 解析支持（`UnitKind::Swap`），但无对应 worker |

---

## 九、平台抽象

| 功能 | 状态 | 说明 |
|------|------|------|
| Linux 平台（主要目标） | ⚠️ 部分实现 | System A 本身无平台相关代码；System S 使用 tokio::process（跨平台），但 SIGTERM 使用 nix（Linux 专用） |
| 统一 `ProcessHandle` 接口 | ❌ 未实现 | 尚无平台抽象层 |
| Windows 平台支持 | ❌ 未实现 | Phase 4 功能 |

---

## 十、测试覆盖

| 测试项 | 状态 | 说明 |
|--------|------|------|
| Unit 文件解析单元测试（`.service`） | ✅ 完全实现 | |
| Unit 文件解析单元测试（`.target`） | ✅ 完全实现 | |
| 依赖图拓扑排序测试 | ✅ 完全实现 | |
| 任务调度器测试（JobMode、start/stop order、条件检查、限流等） | ✅ 完全实现 | 数十个单元测试覆盖所有调度功能（当前 sysa 共 246 个测试） |
| IPC 客户端-服务器集成测试 | ❌ 未实现 | |
| D-Bus 接口集成测试 | ❌ 未实现 | |
| 端到端测试（`systemctl start/stop`） | ⚠️ 部分实现 | 自动化脚本未落地；已在 Linux VM 上手动冒烟验证（M3，全链路 + 物化机制） |
