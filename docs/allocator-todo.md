# System Allocator (System A) 功能实现 TODO 列表

> 根据软件工程设计图与问题陈述整理，对照当前 `crates/system-a` 代码的实际实现状态。
>
> **状态说明：**
> - ✅ **完全实现**：功能与设计目标完全一致
> - ⚠️ **部分实现**：已有代码，但功能不完整或存在偏差（附说明）
> - ❌ **未实现**：功能尚不存在

---

## 一、Unit 加载器（Unit Loader）

**实现文件：** `crates/system-a/src/unit/`

### 1.1 Unit 文件解析

| 功能 | 状态 | 说明 |
|------|------|------|
| INI 格式解析（大小写不敏感的 section/key） | ✅ 完全实现 | 基于 `configparser` |
| `[Unit]` 节解析（Description, After, Before, Requires, Wants, Conflicts, PartOf, BindsTo） | ✅ 完全实现 | |
| `[Install]` 节解析（WantedBy, RequiredBy, Also, Alias） | ✅ 完全实现 | |
| `[Service]` 节解析（ExecStart, ExecStop, ExecReload, Type, Restart, User, Group, WorkingDirectory 等） | ✅ 完全实现 | 支持主要字段 |
| `.service` 单元类型识别 | ✅ 完全实现 | |
| `.target` 单元类型识别 | ✅ 完全实现 | |
| `.mount` 单元类型识别（数据结构） | ⚠️ 部分实现 | 枚举已定义，但无 `MountSection` 解析 |
| `.timer` 单元类型识别（数据结构） | ⚠️ 部分实现 | 枚举已定义，但无 `TimerSection` 解析 |
| `.socket` 单元类型识别（数据结构） | ⚠️ 部分实现 | 枚举已定义，但无 `SocketSection` 解析 |
| `.swap` / `.path` / `.slice` / `.scope` / `.device` | ❌ 未实现 | 枚举中无对应类型 |
| Drop-in 覆盖文件支持（`xxx.service.d/*.conf`） | ❌ 未实现 | |
| `%n` / `%p` / `%i` 等 specifier 展开 | ❌ 未实现 | ExecStart 中的 % 变量不被展开 |
| 条件检查（`ConditionPathExists=`, `ConditionHost=` 等） | ⚠️ 部分实现 | `ConditionPathExists` 字段已解析，但启动时从不执行检查 |
| 断言检查（`AssertPathExists=` 等） | ❌ 未实现 | |
| 多行值（续行符 `\`） | ❌ 未实现 | |
| `ExecStart=-/cmd`（前缀 `-` 允许失败） | ❌ 未实现 | |
| `ExecStart=+/cmd`（前缀 `+` 赋予 root 权限） | ❌ 未实现 | |
| `@`、`:`、`!`、`!!` 命令前缀处理 | ❌ 未实现 | |

### 1.2 Unit 文件搜索与加载

| 功能 | 状态 | 说明 |
|------|------|------|
| 搜索路径列表（7个标准路径） | ✅ 完全实现 | `/etc/systemd/system`、`/run/systemd/system` 等 |
| 按需加载（`GetUnit`/`LoadUnit` 触发） | ✅ 完全实现 | |
| 启动时批量扫描加载所有单元文件 | ⚠️ 部分实现 | 仅加载存在的文件，不扫描整个目录 |
| 单元文件变更监控（inotify）并自动重新加载 | ❌ 未实现 | |
| 单元生成器（Generator）支持（`/lib/systemd/system-generators/`） | ❌ 未实现 | |
| 临时单元（Transient Unit）注册 | ❌ 未实现 | |
| 单元卸载（从内存中移除不再需要的单元） | ❌ 未实现 | 单元只会累积，不会被卸载 |
| 单元别名（Alias）解析 | ❌ 未实现 | |
| 单元遮蔽（Mask，链接到 `/dev/null`）识别 | ❌ 未实现 | |

---

## 二、依赖图构建（Dependency Graph）

**实现文件：** `crates/system-a/src/graph/mod.rs`

| 功能 | 状态 | 说明 |
|------|------|------|
| 基于 petgraph 的有向无环图（DAG）构建 | ✅ 完全实现 | |
| `After=` / `Before=` 排序依赖边 | ✅ 完全实现 | |
| `Requires=` / `Wants=` 语义记录 | ✅ 完全实现 | 记录在 `UnitSection` 中 |
| 未知单元的占位节点 | ✅ 完全实现 | |
| 拓扑排序（检测循环依赖） | ✅ 完全实现 | |
| `Requisite=` 依赖处理（前置依赖，必须已激活） | ❌ 未实现 | `Requisite` 字段未在 `UnitSection` 中建模 |
| `BindsTo=` 依赖的实际生命周期绑定 | ❌ 未实现 | 字段已解析但调度时不强制执行 |
| `PartOf=` 的停止传播 | ❌ 未实现 | 字段已解析但停止时不传播 |
| `Upholds=` 持续维持激活 | ❌ 未实现 | |
| `Conflicts=` 冲突处理（启动时停止冲突单元） | ❌ 未实现 | |
| `OnSuccess=` / `OnFailure=` 触发 | ❌ 未实现 | |
| `PropagatesReloadTo=` 重载传播 | ❌ 未实现 | |
| 反向依赖的停止传播（停止 A 时停止所有依赖 A 的 B） | ❌ 未实现 | |
| 循环依赖自动打破（systemd 的 cycle-breaking 机制） | ❌ 未实现 | 检测到循环直接报错 |

---

## 三、拓扑排序与任务生成（Task Generator）

**实现文件：** `crates/system-a/src/scheduler/mod.rs`

| 功能 | 状态 | 说明 |
|------|------|------|
| 启动：拓扑排序生成 `WorkerTask` 列表 | ✅ 完全实现 | BFS + DFS 展开依赖，按 After/Before 排序 |
| 停止：生成 Stop 任务派发给 Worker | ✅ 完全实现 | 仅停止被请求的单元 |
| 重启：生成 Restart 任务 | ✅ 完全实现 | |
| 重载：生成 Reload 任务 | ✅ 完全实现 | |
| Target 单元内部激活（无需外部 Worker） | ✅ 完全实现 | |
| `task_id → JobKind` 映射（正确更新 ActiveState） | ✅ 完全实现 | |
| 任务结果处理（`handle_task_result`） | ✅ 完全实现 | |
| Job 模式（mode 参数：`replace`/`fail`/`queue`/`isolate`/`flush`） | ❌ 未实现 | mode 参数被完全忽略，始终行为如 `replace` |
| 作业冲突检测（已有同类作业时的行为） | ❌ 未实现 | 同一单元可被多次派发任务 |
| `isolate` 模式（启动目标并停止所有其他单元） | ❌ 未实现 | |
| `ignore-dependencies` 模式 | ❌ 未实现 | |
| `ignore-requirements` 模式 | ❌ 未实现 | |
| 停止时传播：停止依赖下游单元 | ❌ 未实现 | 只停止被请求的单元，不传播 |
| 串行任务执行（等待前一个完成再派下一个） | ❌ 未实现 | 所有任务同时派发，未按依赖顺序等待 |
| 启动超时监控（`TimeoutStartSec=`） | ❌ 未实现 | System A 侧无超时监控 |
| 作业运行超时（`JobRunningTimeoutUSec=`） | ❌ 未实现 | |
| 事件总线：`process.exit` 触发自动重启 | ⚠️ 部分实现 | 接收到 `process.exit` 事件会更新状态为 inactive，但不触发重启 |
| 重启策略执行（`Restart=on-failure/always`） | ❌ 未实现 | Phase 2 功能 |
| 重启限流（`StartLimitIntervalSec=`/`StartLimitBurst=`） | ❌ 未实现 | |
| `D-Bus JobNew / JobRemoved` 信号发出 | ❌ 未实现 | |

---

## 四、期望状态管理（Desired State Manager）

**实现文件：** `crates/system-a/src/state.rs`

| 功能 | 状态 | 说明 |
|------|------|------|
| `desired: HashMap<String, DesiredState>` 期望状态存储 | ✅ 完全实现 | |
| `runtime: HashMap<String, UnitRuntimeInfo>` 运行时状态存储 | ✅ 完全实现 | |
| `ActiveState` 枚举（Inactive/Activating/Active/Deactivating/Failed/Reloading） | ✅ 完全实现 | |
| `JobKind` 枚举（Start/Stop/Restart/Reload） | ✅ 完全实现 | |
| `JobStatus` 枚举（Waiting/Running/Done/Failed/Cancelled） | ✅ 完全实现 | |
| `JobResultKind` 枚举（Done/Failed/Cancelled/Timeout/Dependency/Skipped） | ✅ 完全实现 | |
| Job 完成通知（one-shot channel `completion_tx`） | ⚠️ 部分实现 | 字段已定义，但 D-Bus 层不使用（调用者不等待 job 完成） |
| Worker 注册表（`workers: HashMap`） | ✅ 完全实现 | |
| `task_kinds: HashMap<u64, JobKind>` task 与 job 映射 | ✅ 完全实现 | |
| 系统崩溃后恢复：与 Worker 重新同步状态 | ❌ 未实现 | System A 重启后 runtime 状态清空，Worker 状态不同步 |
| 单元引用计数（`RefUnit`/`UnrefUnit`） | ❌ 未实现 | |
| 期望状态与实际状态对账（reconciliation loop） | ❌ 未实现 | 无持续对账机制 |
| `InvocationID`（每次启动的唯一 UUID） | ❌ 未实现 | |

---

## 五、IPC 服务器（内部通信）

**实现文件：** `crates/system-a/src/ipc/server.rs`

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
| Worker 断线后自动重新连接（System A 视角） | ❌ 未实现 | Worker 断线后仅注销，不尝试重新连接 |
| 多个同类型 Worker 负载均衡 | ❌ 未实现 | 仅取第一个匹配的 Worker |
| 事件总线 Pub/Sub 机制（任意订阅者） | ❌ 未实现 | 当前仅 System A 处理事件，无通用发布订阅 |
| `SOCK_SEQPACKET` 传输层（当前为 `SOCK_STREAM`） | ❌ 未实现 | 当前使用 `UnixListener`（SOCK_STREAM）而非设计中的 SEQPACKET |
| 反压控制（背压机制） | ❌ 未实现 | 任务通道满时直接丢弃 |

---

## 六、D-Bus 服务器（外部兼容层）

**实现文件：** `crates/system-a/src/dbus/`

| 功能 | 状态 | 说明 |
|------|------|------|
| 声明 `org.freedesktop.systemd1` 总线名称 | ✅ 完全实现 | |
| `org.freedesktop.systemd1.Manager` 接口注册（`/org/freedesktop/systemd1`） | ✅ 完全实现 | |
| 核心管理方法（Start/Stop/Restart/Reload/ListUnits 等） | ⚠️ 部分实现 | 详见 `dbus-todo.md` |
| 管理器属性（Version/Architecture/NNames 等） | ⚠️ 部分实现 | 大量属性为固定值，详见 `dbus-todo.md` |
| 每单元 D-Bus 对象动态注册（`Unit`/`Service`/`Target` 接口） | ❌ 未实现 | `unit_obj.rs` 仅为占位注释 |
| 每作业 D-Bus 对象动态注册（`Job` 接口） | ❌ 未实现 | |
| D-Bus 信号（`UnitNew`/`UnitRemoved`/`JobNew`/`JobRemoved`/`StartupFinished`） | ❌ 未实现 | |
| polkit 权限检查 | ❌ 未实现 | 所有方法无访问控制 |
| `org.freedesktop.DBus.Properties` 标准接口 | ⚠️ 部分实现 | zbus 自动为 Manager 生成，但单元对象未注册 |
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
| **System S**（Service Worker） | ⚠️ 部分实现 | IPC 通信已实现；System S 已实现基础状态机和进程管理；重启策略、cgroup、看门狗等未实现 |
| **System T**（Target Worker） | ⚠️ 部分实现 | Target 内部激活逻辑已内嵌于 System A，无独立 Worker；但不能响应来自 Target 的事件 |
| **System M**（Mount Worker） | ❌ 未实现 | `crates/system-m` 不存在 |
| **System C**（Cron/Timer Worker） | ❌ 未实现 | `crates/system-c` 不存在 |
| **System B**（Boot Worker） | ❌ 未实现 | `crates/system-b` 不存在 |

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
| 依赖图拓扑排序测试 | ❌ 未实现 | |
| 任务调度器测试 | ❌ 未实现 | |
| IPC 客户端-服务器集成测试 | ❌ 未实现 | |
| D-Bus 接口集成测试 | ❌ 未实现 | |
| 端到端测试（`systemctl start/stop`） | ❌ 未实现 | |
