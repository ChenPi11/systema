# D-Bus 接口实现 TODO 列表

> 对照 [systemd D-Bus 接口文档](https://github.com/ChenPi11/systemd/tree/main/i-doc/system_bus) 整理。
>
> **状态说明：**
> - ✅ **完全实现**：入参、返回值、实际行为与 systemd 完全相同
> - ⚠️ **部分实现**：已有代码，但行为、返回值或参数处理与 systemd 不同（附说明）
> - ❌ **未实现**：接口/方法/属性不存在或返回空/错误
>
> **最后核对：** 2026-08-16（M1/M2 及冒烟修复后），实现文件位于
> `crates/backend/systema-sysa/src/dbus/`（manager.rs / mod.rs / properties.rs /
> unit_obj.rs / service_obj.rs / scope_obj.rs / slice_obj.rs / mount_obj.rs / socket_obj.rs）。
>
> **⚠️ 全局问题 1——方法名大小写：** zbus 4.4 的 `#[interface]` 宏自动把
> `get_unit_by_pid` 转成 PascalCase `GetUnitByPid`（不是 systemd 的
> `GetUnitByPID`），`get_unit_by_pidfd` → `GetUnitByPidfd`、
> `get_unit_by_invocation_id` → `GetUnitByInvocationId`。D-Bus 方法名大小写敏感，
> 按 systemd 名称调用这些方法的客户端会收到 `UnknownMethod`。需在
> `#[zbus(name = "GetUnitByPID")]` 等显式覆盖。
>
> **⚠️ 全局问题 2——polkit 权限检查：** 所有管理方法均无访问控制
> （systemd 依赖 polkit 授权普通用户执行 `systemctl` 操作）。

---

## 接口继承层级概览

```
org.freedesktop.systemd1.Manager         （路径：/org/freedesktop/systemd1）
org.freedesktop.systemd1.Unit            （路径：/org/freedesktop/systemd1/unit/<name>，基础接口）
    ├── org.freedesktop.systemd1.Service  （服务单元专有接口）✅ 已注册
    ├── org.freedesktop.systemd1.Target   （目标单元专有接口，无额外方法/属性）❌ 未注册
    ├── org.freedesktop.systemd1.Mount    （挂载单元）✅ 已注册
    ├── org.freedesktop.systemd1.Automount（自动挂载单元）❌ 未注册
    ├── org.freedesktop.systemd1.Socket   （套接字单元）✅ 已注册
    ├── org.freedesktop.systemd1.Timer    （定时器单元）❌ 未注册
    ├── org.freedesktop.systemd1.Path     （路径监控单元）❌ 未注册
    ├── org.freedesktop.systemd1.Swap     （交换分区单元）❌ 未注册
    ├── org.freedesktop.systemd1.Slice    （资源切片单元）✅ 已注册
    ├── org.freedesktop.systemd1.Scope    （作用域单元）✅ 已注册（⚠️ 属性不可达，见 3.3 节）
    └── org.freedesktop.systemd1.Device   （设备单元）❌ 未注册
org.freedesktop.systemd1.Job             （路径：/org/freedesktop/systemd1/job/<id>）❌ 整个接口未实现
```

---

## 一、`org.freedesktop.systemd1.Manager`

对象路径：`/org/freedesktop/systemd1`
实现文件：`crates/backend/systema-sysa/src/dbus/manager.rs`

### 1.1 方法（Methods）——已实现（36 个）

#### 单元查询

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `GetUnit(name)` | ⚠️ 部分实现 | 只支持非空 name；systemd 支持传空字符串返回调用方所属单元，未实现；未加载时返回 `UnknownObject`（systemctl 据此回退 `LoadUnit`，行为与 systemd 一致） |
| `GetUnitByPID(pid)` | ⚠️ 命名不符 | M2 已实现（scope 的 `CachedUnitState.pids` / service 的 main PID）；但线上名称为 `GetUnitByPid`，见全局问题 1 |
| `GetUnitByPIDFD(pidfd)` | ⚠️ 命名不符 | M2 已实现（经 `/proc/self/fdinfo` 的 `Pid:` 行解析）；线上名称为 `GetUnitByPidfd`，见全局问题 1 |
| `GetUnitByInvocationID(id)` | ⚠️ 命名不符 | 扫描 `runtime` 表中匹配的 `invocation_id`，返回单元对象路径；线上名称为 `GetUnitByInvocationId`，见全局问题 1 |
| `GetUnitByControlGroup(cgroup)` | ❌ 未实现 | 按 cgroup 路径查找单元 |
| `LoadUnit(name)` | ⚠️ 部分实现 | 功能正确，但使用 `spawn_blocking` 同步加载；无 polkit 权限检查 |

#### 单元操作

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `StartUnit(name, mode)` | ⚠️ 部分实现 | `mode` 已完整解析并遵守（`parse_job_mode`：`fail`/`lenient`/`replace`/`replace-irreversibly`/`isolate`/`flush`/`ignore-dependencies`/`ignore-requirements`/`triggering`/`restart-dependencies` + 遗留 `queue`）；无 polkit |
| `StartUnitWithFlags(name, mode, flags)` | ❌ 未实现 | 带 flags 标志位的启动方法 |
| `StartUnitReplace(old_unit, new_unit, mode)` | ❌ 未实现 | 替换等待中的旧单元任务 |
| `StopUnit(name, mode)` | ⚠️ 部分实现 | `mode` 已解析；停止时传播到下游依赖单元；无 polkit |
| `ReloadUnit(name, mode)` | ⚠️ 部分实现 | `mode` 已解析；无 polkit |
| `RestartUnit(name, mode)` | ⚠️ 部分实现 | `mode` 已解析；无 polkit |
| `TryRestartUnit(name, mode)` | ⚠️ 部分实现 | 经 `enqueue_transient` 真实作业入队（未运行时以 no-op 完成）；无 polkit |
| `TryReloadUnit(name, mode)` | ⚠️ 部分实现 | `systemctl try-reload` 语义；无 polkit |
| `ReloadOrRestartUnit(name, mode)` | ⚠️ 部分实现 | 无 polkit |
| `ReloadOrTryRestartUnit(name, mode)` | ⚠️ 部分实现 | 支持重载则重载，否则仅在运行时才重启；无 polkit |
| `EnqueueUnitJob(name, job_type, job_mode)` | ⚠️ 部分实现 | 返回详细受影响作业信息列表；无 polkit |
| `EnqueueUnitJobMany(jobs)` | ⚠️ 部分实现 | 批量版本；无 polkit |
| `KillUnit(name, whom, signal)` | ❌ 未实现 | **M2 审计结论：全链路缺口**——无 D-Bus 方法、`UnitController` trait 无 `kill` 方法、worker 无信号处理（需扩展 trait + 信封协议） |
| `KillUnitSubgroup(name, whom, subgroup, signal)` | ❌ 未实现 | 向单元 cgroup 子组发送信号 |
| `QueueSignalUnit(name, whom, signal, value)` | ❌ 未实现 | 发送带附加值的实时信号（sigqueue） |
| `CleanUnit(name, mask)` | ❌ 未实现 | 清理运行时文件、日志、状态等资源 |
| `FreezeUnit(name)` | ❌ 未实现 | 冻结单元（暂停 cgroup 中所有进程） |
| `ThawUnit(name)` | ❌ 未实现 | 解冻单元 |
| `ResetFailedUnit(name)` | ⚠️ 部分实现 | 功能基本正确；单元不存在时静默返回 OK |
| `SetUnitProperties(name, runtime, properties)` | ⚠️ 部分实现 | M2 已实现：仅支持资源限制属性（memory/CPU/IO/tasks，`cgroup.resource_control`），仅 `replace` 语义；无 polkit |
| `BindMountUnit(name, source, destination, read_only, mkdir)` | ❌ 未实现 | 向服务命名空间添加绑定挂载 |
| `MountImageUnit(name, source, destination, read_only, mkdir, options)` | ❌ 未实现 | 向服务命名空间挂载磁盘镜像 |
| `RefUnit(name)` | ✅ 完全实现 | 增加 `UnitRuntimeInfo.n_ref`，返回新引用计数 |
| `UnrefUnit(name)` | ✅ 完全实现 | 减少 `UnitRuntimeInfo.n_ref`，返回新引用计数 |
| `StartTransientUnit(name, mode, properties, aux)` | ⚠️ 部分实现 | M1 已实现：动态创建并启动临时单元（经 `unit.define` 物化机制落盘到 worker）；无 polkit |
| `StartTransientUnitMany(units)` | ⚠️ 部分实现 | 批量版本；无 polkit |

#### 进程与作用域

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `GetUnitProcesses(name)` | ⚠️ 部分实现 | 已实现，数据来自 System R 推送的 `cgroup.metrics` 缓存；返回 `(cgroup_path, pid, cmdline)` |
| `AttachProcessesToUnit(name, cgroup_path, pids)` | ❌ 未实现 | 将进程附加到单元的 cgroup |
| `RemoveSubgroupFromUnit(name, subgroup)` | ❌ 未实现 | 移除单元的 cgroup 子组 |
| `AbandonScope(name)` | ✅ 完全实现 | 放弃 scope 单元（M1） |

#### 作业管理

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `GetJob(id)` | ❌ 未实现 | 获取指定作业的 D-Bus 对象路径（`Job` 对象未注册，见第五节） |
| `GetJobAfter(id)` | ❌ 未实现 | 获取在此作业之后运行的作业列表 |
| `GetJobBefore(id)` | ❌ 未实现 | 获取在此作业之前必须完成的作业列表 |
| `CancelJob(id)` | ❌ 未实现 | 取消等待中的作业 |
| `ClearJobs()` | ❌ 未实现 | 清除所有等待中的作业 |

#### 系统管理

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `ResetFailed()` | ⚠️ 部分实现 | 功能基本正确，无权限检查 |
| `SetShowStatus(mode)` | ❌ 未实现 | 动态修改状态显示模式 |
| `Reload()` | ⚠️ 部分实现 | 已实现：发出 `Reloading` 信号 → 重扫单元文件 → 请求 worker 同步；不重新执行自身（`Reexecute`），异步执行无错误回报 |
| `Reexecute()` | ❌ 未实现 | 重新执行守护进程自身（热重启） |
| `Exit()` | ❌ 未实现 | 退出 systemd（仅限用户实例） |
| `Reboot()` / `SoftReboot()` / `PowerOff()` / `Halt()` / `KExec()` | ❌ 未实现 | 系统电源管理（System B 未实现）；目前仅 `StartLimitAction=` 副作用会调用外部 `shutdown` 命令 |
| `SwitchRoot(new_root, init)` | ❌ 未实现 | 切换根文件系统（initrd → 真实根） |
| `EnqueueMarkedJobs()` | ❌ 未实现 | 将标记的单元加入作业队列 |

#### 环境变量管理

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `SetEnvironment(assignments)` | ❌ 未实现 | 设置系统环境变量 |
| `UnsetEnvironment(names)` | ❌ 未实现 | 取消设置系统环境变量 |
| `UnsetAndSetEnvironment(unset, set)` | ❌ 未实现 | 原子性取消并设置环境变量 |

#### 单元列表

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `ListUnits()` | ⚠️ 部分实现 | 仅包含已加载单元；`following` 字段固定为空（manager.rs:1879） |
| `ListUnitsFiltered(states)` | ⚠️ 部分实现 | 按状态过滤列出单元；空 states = 全部 |
| `ListUnitsByPatterns(states, patterns)` | ⚠️ 部分实现 | 按状态和通配符模式列出单元 |
| `ListUnitsByNames(names)` | ⚠️ 部分实现 | 按名称列出单元（包括未加载的） |
| `ListJobs()` | ⚠️ 部分实现 | 只返回 `Running` 状态的作业；格式基本正确 |

#### 订阅与转储

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `Subscribe()` | ⚠️ 部分实现 | no-op（不维护订阅者集合，信号对所有人广播） |
| `Unsubscribe()` | ⚠️ 部分实现 | no-op |
| `Dump()` | ❌ 未实现 | 返回系统状态的文本转储（**systemd-analyze 依赖**） |
| `DumpUnitsMatchingPatterns(patterns)` | ❌ 未实现 | 按模式转储单元状态 |
| `DumpByFileDescriptor(fd)` | ❌ 未实现 | 通过 FD 输出转储 |
| `DumpUnitsMatchingPatternsByFileDescriptor(patterns, fd)` | ❌ 未实现 | 按模式通过 FD 输出转储 |

#### 单元文件管理

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `ListUnitFiles()` | ⚠️ 部分实现 | 只列出已加载进内存的单元，不扫描磁盘所有单元文件；路径用配置的库目录（`systemd_lib_unit_dir`）拼接；`enabled`/`disabled`/`static` 判断基于 `[Install]` 节与启用符号链接（`unit_file_state`） |
| `ListUnitFilesByPatterns(states, patterns)` | ⚠️ 部分实现 | 按状态和模式列出单元文件 |
| `GetUnitFileState(name)` | ⚠️ 部分实现 | 已实现：已加载单元按 `[Install]` 判断；磁盘存在但未加载的报告 `"static"`；不存在的报错 |
| `EnableUnitFiles(names, runtime, force)` | ❌ 未实现 | 启用单元文件（创建符号链接） |
| `DisableUnitFiles(names, runtime)` | ❌ 未实现 | 禁用单元文件（删除符号链接） |
| `EnableUnitFilesWithFlags(names, flags)` | ❌ 未实现 | 带标志位的启用 |
| `DisableUnitFilesWithFlags(names, flags)` | ❌ 未实现 | 带标志位的禁用 |
| `DisableUnitFilesWithFlagsAndInstallInfo(names, flags)` | ❌ 未实现 | 带标志位禁用并返回安装信息 |
| `ReenableUnitFiles(names, runtime, force)` | ❌ 未实现 | 重新启用单元文件 |
| `LinkUnitFiles(paths, runtime, force)` | ❌ 未实现 | 链接非标准位置的单元文件 |
| `PresetUnitFiles(names, runtime, force)` | ❌ 未实现 | 按预设策略启用/禁用 |
| `PresetUnitFilesWithMode(names, mode, runtime, force)` | ❌ 未实现 | 指定模式的预设 |
| `PresetAllUnitFiles(mode, runtime, force)` | ❌ 未实现 | 对所有单元应用预设策略 |
| `MaskUnitFiles(names, runtime, force)` | ❌ 未实现 | 屏蔽单元文件 |
| `UnmaskUnitFiles(names, runtime)` | ❌ 未实现 | 取消屏蔽 |
| `RevertUnitFiles(names)` | ❌ 未实现 | 还原单元文件到原始状态 |
| `AddDependencyUnitFiles(names, target, type, runtime, force)` | ❌ 未实现 | 为单元文件添加依赖 |
| `GetUnitFileLinks(name, runtime)` | ❌ 未实现 | 获取单元文件的所有符号链接 |
| `SetDefaultTarget(name, force)` | ❌ 未实现 | 设置默认启动目标 |
| `GetDefaultTarget()` | ❌ 未实现 | 获取当前默认启动目标 |

#### 其他

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `SetExitCode(code)` | ❌ 未实现 | 设置守护进程退出码 |
| `LookupDynamicUserByName(name)` | ❌ 未实现 | 按名称查找动态用户 UID |
| `LookupDynamicUserByUID(uid)` | ❌ 未实现 | 按 UID 查找动态用户名 |
| `GetDynamicUsers()` | ❌ 未实现 | 获取所有动态用户列表 |
| `DumpUnitFileDescriptorStore(name)` | ❌ 未实现 | 转储单元的 FD 存储内容 |

### 1.2 属性（Properties）

共注册 63 个属性（manager.rs:1491-1819），绝大多数为固定值。

#### 系统信息属性

| 属性名 | 状态 | 说明 |
|--------|------|------|
| `Version` | ⚠️ 部分实现 | 固定返回 `"255"`（版本检查兼容） |
| `Features` | ⚠️ 部分实现 | 固定返回空字符串，systemd 中为编译特性列表 |
| `Virtualization` | ⚠️ 部分实现 | 固定返回空字符串，未检测虚拟化环境 |
| `Architecture` | ✅ 完全实现 | 通过 `std::env::consts::ARCH` 返回正确架构名 |
| `Tainted` | ⚠️ 部分实现 | 固定返回空字符串 |
| `ManagerState` | ❌ 未实现 | 系统管理器状态（`initializing`/`starting`/`running`/`degraded`/`maintenance`/`stopping`） |

#### 启动时间戳属性

均已注册，但全部固定返回 `0`（未接入实际时间测量）：

| 属性名 | 状态 |
|--------|------|
| `FirmwareTimestamp` / `FirmwareTimestampMonotonic` | ⚠️ 部分实现（固定0） |
| `LoaderTimestamp` / `LoaderTimestampMonotonic` | ⚠️ 部分实现（固定0） |
| `KernelTimestamp` / `KernelTimestampMonotonic` | ⚠️ 部分实现（固定0） |
| `InitRDTimestamp` / `InitRDTimestampMonotonic` | ⚠️ 部分实现（固定0） |
| `UserspaceTimestamp` / `UserspaceTimestampMonotonic` | ⚠️ 部分实现（固定0） |
| `FinishTimestamp` / `FinishTimestampMonotonic` | ⚠️ 部分实现（固定0） |
| `SecurityStartTimestamp` / `SecurityFinishTimestamp`（及 Monotonic） | ⚠️ 部分实现（固定0） |
| `GeneratorsStartTimestamp` / `GeneratorsFinishTimestamp`（及 Monotonic） | ⚠️ 部分实现（固定0） |
| `UnitsLoadStartTimestamp` / `UnitsLoadFinishTimestamp`（及 Monotonic） | ⚠️ 部分实现（固定0） |
| `ShutdownStartTimestamp` / `ShutdownFinishTimestamp`（及 Monotonic） | ⚠️ 部分实现（固定0） |
| InitRD 系列（`InitRDGenerators*`、`InitRDUnitsLoad*` 等） | ⚠️ 部分实现（固定0） |

#### 日志控制属性

| 属性名 | 状态 | 说明 |
|--------|------|------|
| `LogLevel` | ⚠️ 部分实现 | 固定返回 `"info"`，不可动态设置 |
| `LogTarget` | ⚠️ 部分实现 | 固定返回 `"journal"`，不可动态设置（无 journal 后端） |
| `LogColor` | ❌ 未实现 | 日志颜色开关 |
| `LogLocation` | ❌ 未实现 | 日志位置信息开关 |
| `LogTime` | ❌ 未实现 | 日志时间显示开关 |

#### 统计属性

| 属性名 | 状态 | 说明 |
|--------|------|------|
| `NNames` | ✅ 完全实现 | 实际返回已加载单元数量 |
| `NFailedUnits` | ⚠️ 部分实现 | 固定返回 `0`（未统计 Failed 单元） |
| `NJobs` | ✅ 完全实现 | 实际统计 Running 作业数 |
| `NInstalledJobs` | ⚠️ 部分实现 | 固定返回 `0` |
| `NFailedJobs` | ⚠️ 部分实现 | 固定返回 `0` |
| `Progress` | ⚠️ 部分实现 | 固定返回 `1.0`，不反映真实启动进度 |

#### 环境与配置属性

| 属性名 | 状态 | 说明 |
|--------|------|------|
| `Environment` | ✅ 完全实现 | 实际读取进程环境变量 |
| `ConfirmSpawn` | ⚠️ 部分实现 | 固定返回 `false`，不可设置 |
| `ShowStatus` | ⚠️ 部分实现 | 固定返回 `true`，不可通过 `SetShowStatus` 修改 |
| `UnitPath` | ✅ 完全实现 | 返回实际的单元文件搜索路径列表 |
| `DefaultStandardOutput` | ⚠️ 部分实现 | 固定返回 `"journal"`（无 journal 后端） |
| `DefaultStandardError` | ⚠️ 部分实现 | 固定返回 `"journal"`（无 journal 后端） |
| `ControlGroup` | ⚠️ 部分实现 | 固定返回 `"/"`，未接入 cgroup 管理 |
| `SystemState` | ⚠️ 部分实现 | 固定返回 `"running"`，不反映真实系统状态（如 degraded） |
| `ExitCode` | ⚠️ 部分实现 | 固定返回 `0`，无法通过 `SetExitCode` 修改 |

#### 看门狗属性

| 属性名 | 状态 | 说明 |
|--------|------|------|
| `RuntimeWatchdogUSec` | ⚠️ 部分实现 | 固定返回 `0`，无实际看门狗功能 |
| `RebootWatchdogUSec` | ⚠️ 部分实现 | 固定返回 `0` |
| `KExecWatchdogUSec` | ⚠️ 部分实现 | 固定返回 `0` |
| `ServiceWatchdogs` | ⚠️ 部分实现 | 固定返回 `true`，无实际看门狗逻辑 |

#### 默认超时与资源属性

| 属性名 | 状态 | 说明 |
|--------|------|------|
| `DefaultTimerAccuracyUSec` | ⚠️ 部分实现 | 固定返回 60s，不可配置 |
| `DefaultTimeoutStartUSec` | ⚠️ 部分实现 | 固定返回 90s，不可配置 |
| `DefaultTimeoutStopUSec` | ⚠️ 部分实现 | 固定返回 90s，不可配置 |
| `DefaultTimeoutAbortUSec` | ⚠️ 部分实现 | 固定返回 90s，不可配置 |
| `DefaultRestartUSec` | ⚠️ 部分实现 | 固定返回 100ms，不可配置 |
| `DefaultStartLimitIntervalUSec` | ⚠️ 部分实现 | 固定返回 10s，不可配置 |
| `DefaultStartLimitBurst` | ⚠️ 部分实现 | 固定返回 5，不可配置 |
| `DefaultTasksMax` | ⚠️ 部分实现 | 固定返回 `u64::MAX` |
| `TimerSlackNSec` | ⚠️ 部分实现 | 固定返回 50000ns |

#### 默认资源限制属性（RLIMIT_*）

| 属性名 | 状态 |
|--------|------|
| `DefaultLimitCPU` / `DefaultLimitCPUSoft` | ⚠️ 部分实现（固定 u64::MAX） |
| `DefaultLimitFSIZE` / `...Soft` 等其余全部 RLIMIT | ❌ 未实现 |

#### 默认计账属性

| 属性名 | 状态 | 说明 |
|--------|------|------|
| `DefaultCPUAccounting` | ⚠️ 部分实现 | 固定返回 `false` |
| `DefaultBlockIOAccounting` | ⚠️ 部分实现 | 固定返回 `false` |
| `DefaultMemoryAccounting` | ⚠️ 部分实现 | 固定返回 `true` |
| `DefaultTasksAccounting` | ⚠️ 部分实现 | 固定返回 `true` |
| `DefaultIOAccounting` | ❌ 未实现 |  |
| `DefaultIPAccounting` | ❌ 未实现 |  |
| `DefaultMemoryPressureThresholdUSec` / `DefaultMemoryPressureWatch` | ❌ 未实现 |  |

#### 其他已注册 / 未注册属性

| 属性名 | 状态 | 说明 |
|--------|------|------|
| `KExecsCount` / `ReloadCount` | ⚠️ 部分实现 | 固定返回 `0` |
| `EventLoopRateLimitIntervalUSec` / `EventLoopRateLimitBurst` | ⚠️ 部分实现 | 固定返回 1s / 50000 |
| `CPUSetPartition` | ⚠️ 部分实现 | 固定返回 `"member"` |
| `OOMRules` | ⚠️ 部分实现 | 固定返回空数组 |
| `DefaultCPUWeight` / `DefaultIOWeight` / `DefaultOOMScoreAdjust` / `DefaultDeviceTimeoutUSec` / `DefaultStartLimitAction` 等 | ❌ 未实现 | 未注册 |

### 1.3 信号（Signals）

| 信号名 | 状态 | 说明 |
|--------|------|------|
| `UnitNew(id, unit)` | ❌ 未实现 | 单元对象注册时不发出（dbus/mod.rs:359-366） |
| `UnitRemoved(id, unit)` | ✅ 完全实现 | M2 已实现（`unit_removed_tx` 通道 + dbus/mod.rs:368-389） |
| `JobNew(id, job, unit)` | ⚠️ 部分实现 | 已发出（`job_new_tx` 通道 + dbus/mod.rs:334-357）；但 `Job` 对象未注册，路径不可解析（同 `JobRemoved`） |
| `JobRemoved(id, job, unit, result)` | ⚠️ 部分实现 | 已在作业完成时发出；但 `Job` 对象本身未注册（路径不可解析） |
| `StartupFinished(firmware, loader, kernel, initrd, userspace, total)` | ❌ 未实现 | 系统启动完成时发出 |
| `UnitFilesChanged()` | ❌ 未实现 | 单元文件变更时发出 |
| `Reloading(active)` | ✅ 完全实现 | M2 已实现（`emit_reloading`，manager.rs:1370/1384） |
| `PropertiesChanged` | ⚠️ 部分实现 | 仅在 introspection XML 中声明（properties.rs:467-496 / 753-782），从不实际发出 |

---

## 二、`org.freedesktop.systemd1.Unit`

对象路径：`/org/freedesktop/systemd1/unit/<escaped_name>`
实现文件：`crates/backend/systema-sysa/src/dbus/unit_obj.rs`

> **当前状态：部分实现。** 所有已注册单元对象均提供基础 `Unit` 接口；属性覆盖面较广
> （69 个），cgroup 资源类属性为实时值；但单元级操作方法基本缺失。

### 2.1 方法（Methods）

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `GetProcesses()` | ⚠️ 部分实现 | 已实现（unit_obj.rs:549），数据来自 System R `cgroup.metrics` 缓存 |
| `GetTriggeringUnits()` | ⚠️ 部分实现 | 已实现（unit_obj.rs:564） |
| `ResetFailed()` | ⚠️ 部分实现 | 可将 `failed` 状态重置为 `inactive/dead`，行为未完全对齐 systemd |
| `Start(mode)` / `Stop(mode)` / `Reload(mode)` / `Restart(mode)` / `TryRestart(mode)` / `ReloadOrRestart(mode)` / `ReloadOrTryRestart(mode)` / `EnqueueJob(job_type, job_mode)` | ❌ 未实现 | 单元级操作仅能通过 Manager 方法 |
| `Kill(whom, signal)` / `KillSubgroup(whom, subgroup, signal)` / `QueueSignal(whom, signal, value)` | ❌ 未实现 | 同 Manager `KillUnit` 缺口 |
| `SetProperties(runtime, properties)` | ❌ 未实现 | 仅 Manager 级 `SetUnitProperties` |
| `Ref()` / `Unref()` | ❌ 未实现 | 仅 Manager 级 `RefUnit`/`UnrefUnit` |
| `Clean(mask)` | ❌ 未实现 | 清理资源 |
| `Freeze()` / `Thaw()` | ❌ 未实现 | 冻结/解冻 |
| `AttachProcesses(cgroup_path, pids)` / `RemoveSubgroup(subgroup)` | ❌ 未实现 | cgroup 进程管理 |

### 2.2 属性（Properties，69 个已实现）

已实现（unit_obj.rs，行号见括号）：

| 类别 | 属性名 | 状态 |
|------|--------|------|
| 标识 | `Id`(28) `Names`(33) `Description`(38) `Documentation`(48) `Following`(92) `FragmentPath`(125) `SourcePath`(136) `UnitFileState`(101) `UnitFilePreset`(120) `Transient`(261) `Perpetual`(271) `NeedDaemonReload`(278) `ActivationDetails`(303) `Refs`(308) `InvocationID`(333) | ⚠️ 部分实现 |
| 状态 | `LoadState`(62) `ActiveState`(72) `SubState`(82) `Job`(145) `CanStart`(162) `CanStop`(167) `CanReload`(172) `CanIsolate`(183) `CanFreeze`(188) `ConditionResult`(293) `AssertResult`(298) `JobTimeoutUSec`(283) `JobRunningTimeoutUSec`(288) | ⚠️ 部分实现 |
| 依赖 | `Requires`(197) `Wants`(207) `After`(217) `Before`(227) `Triggers`(232) `TriggeredBy`(237) `RequiresMountsFor`(242) `PropagatesReloadTo`(247) `ReloadPropagatedFrom`(252) | ⚠️ 部分实现 |
| 时间戳 | `ActiveEnterTimestamp`(313) `InactiveEnterTimestamp`(323) | ⚠️ 部分实现 |
| cgroup 资源（实时） | `MemoryMin`(357) `MemoryLow`(364) `MemoryHigh`(371) `MemoryMax`(378) `MemorySwapMax`(385) `CPUQuotaUSec`(392) `CPUQuotaPeriodUSec`(399) `CPUWeight`(406) `StartupCPUWeight`(413) `IOWeight`(420) `StartupIOWeight`(427) `TasksMax`(434) `AllowedCPUs`(441) `AllowedMemoryNodes`(448) `CPUSetCPUs`(455) `CPUSetMemoryNodes`(462) `ControlGroup`(473) `ControlGroupId`(480) `MemoryCurrent`(487) `MemoryPeak`(492) `MemorySwapCurrent`(497) `CPUUsageNSec`(502) `TasksCurrent`(507) `OOMKills`(512) `IOReadBytes`(517) `IOReadOperations`(522) `IOWriteBytes`(527) `IOWriteOperations`(532) `EffectiveTasksMax`(537) `EffectiveMemoryMax`(542) | ⚠️ 部分实现（值来自状态缓存，与 systemd 读取 cgroup 略有差异） |

仍未实现（非完整列表）：

| 属性类别 | 状态 |
|--------|------|
| 反向依赖与传播类（`RequiredBy`、`RequisiteOf`、`PropagatesStopTo`、`ConflictedBy`、`PartOf`、`OnSuccess`、`OnFailure`、`Upholds`、`BindsTo`、`Conflicts` 等） | ❌ 未实现 |
| 其余时间戳类（`StateChangeTimestamp`、`ActiveExitTimestamp`、`InactiveExitTimestamp` 等） | ❌ 未实现 |
| 作业策略类（`JobTimeoutAction`、`OnSuccessJobMode`、`OnFailureJobMode` 等） | ❌ 未实现 |
| 启停限制类（`StartLimitIntervalUSec`、`StartLimitBurst`、`StartLimitAction`） | ❌ 未实现 |
| 条件明细类（`ConditionTimestamp`、`Conditions`、`Asserts`） | ❌ 未实现 |
| SELinux / CollectMode / FileDescriptorStore 等高级属性 | ❌ 未实现 |

### 2.3 信号（Signals）

Unit 接口本身无额外信号，属性变更通知通过标准 `PropertiesChanged` 信号发出——但
System A 从不实际发出该信号（见 1.3 节）。

---

## 三、`org.freedesktop.systemd1.Service`（继承自 Unit）

对象路径：`/org/freedesktop/systemd1/unit/<service_name>`
实现文件：`crates/backend/systema-sysa/src/dbus/service_obj.rs`

### 3.1 方法（Methods）

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `BindMount(source, destination, read_only, mkdir)` | ❌ 未实现 | 动态绑定挂载到服务命名空间 |
| `MountImage(source, destination, read_only, mkdir, options)` | ❌ 未实现 | 动态挂载镜像到服务命名空间 |
| `DumpFileDescriptorStore()` | ❌ 未实现 | 转储 FD 存储内容 |

以及继承自 `Unit` 的方法（大多未实现，见上节）。

### 3.2 属性（Properties，14 个已实现）

| 属性名 | 状态 |
|--------|------|
| `Type`(30) `MainPID`(41) `ControlPID`(51) `BusName`(56) `ExecMainPID`(67) `ExecMainStatus`(72) `Result`(77) `Restart`(85) `RestartUSec`(96) `NotifyAccess`(107) `RestartRandomizedDelayUSec`(118) `CPUSetPartition`(128) `OOMRules`(133) | ⚠️ 部分实现 |
| `LUOSession`(123) | ⚠️ 部分实现（拼写与 systemd 的 `LUOfSession` 不符） |

仍未实现（systemd 中约 100+ 属性，非完整列表）：

| 属性类别 | 状态 |
|--------|------|
| `ExitType` `RemainAfterExit` `GuessMainPID` `RootDirectoryStartOnly` `OOMPolicy` `PIDFile` | ❌ 未实现 |
| `UID` / `GID` / `User` / `Group` / `SupplementaryGroups` | ❌ 未实现 |
| `TimeoutStartUSec` / `TimeoutStopUSec` / `TimeoutAbortUSec` / `TimeoutStartFailureMode` / `TimeoutStopFailureMode` | ❌ 未实现 |
| `RuntimeMaxUSec` / `RuntimeRandomizedExtraUSec` / `WatchdogUSec` | ❌ 未实现 |
| `NRestarts` / `RestartSteps` / `RestartMaxDelayUSec` / `RestartUSecNext` / `RestartPreventExitStatus` / `RestartForceExitStatus` / `SuccessExitStatus` | ❌ 未实现 |
| `StatusText` / `StatusErrno` / `StatusBusError` / `StatusVarlinkError` | ❌ 未实现 |
| `Result` / `ReloadResult` / `CleanResult` / `LiveMountResult` 的完整状态机 | ⚠️ 部分实现 |
| `FileDescriptorStoreMax` / `NFileDescriptorStore` / `FileDescriptorStorePreserve` | ❌ 未实现 |
| `ExecCondition` / `ExecStartPre` / `ExecStart` / `ExecStartPost` / `ExecReload` / `ExecStop` / `ExecStopPost` | ❌ 未实现 |
| `WorkingDirectory` / `RootDirectory` / `Environment` / `EnvironmentFiles` / `OpenFile` / `ReloadSignal` | ❌ 未实现 |
| cgroup 上下文（CPUAccounting、MemoryMax 等） | ⚠️ 部分实现（Unit 级资源属性已覆盖大部分） |
| 执行上下文（命名空间、安全、能力等，约 100+ 属性） | ❌ 未实现 |

---

## 四、`org.freedesktop.systemd1.Target`（继承自 Unit）

> **当前状态：部分实现。** Target 单元注册基础 `Unit` 对象（Target 由独立的
> System T worker 跟踪状态）；但 `org.freedesktop.systemd1.Target` 专有接口
> （空接口）未注册——不影响属性读取，但 introspection 不完整。

---

## 五、`org.freedesktop.systemd1.Job`

对象路径：`/org/freedesktop/systemd1/job/<job_id>`

> **当前状态：整个接口完全未实现。** 作业的 D-Bus 对象未被动态注册。`ListJobs`、
> `StartUnit` 与 `JobNew`/`JobRemoved` 信号中出现的作业路径仅为引用值，在 D-Bus 上
> 不可解析为实际 `Job` 对象。影响：`systemctl cancel`、`systemctl show <job>` 不可用。

### 5.1 属性（Properties）

| 属性名 | 状态 |
|--------|------|
| `Id` / `Unit` / `JobType` / `State` / `ActivationDetails` | ❌ 未实现 |

### 5.2 方法（Methods）

| 方法名 | 状态 |
|--------|------|
| `Cancel()` / `GetAfter()` / `GetBefore()` | ❌ 未实现 |

---

## 六、其他单元类型接口

| 接口名 | 对应单元类型 | 状态 | 说明 |
|--------|------------|------|------|
| `org.freedesktop.systemd1.Mount` | `.mount` 挂载单元 | ⚠️ 已注册 | 属性 8 个：`Where` `What` `Options` `TimeoutUSec` `ControlPID` `Result` `CPUSetPartition` `OOMRules`（mount_obj.rs）；System M worker 已实现 |
| `org.freedesktop.systemd1.Automount` | `.automount` 自动挂载 | ❌ 未注册 | System M(linux) worker 已实现，但无 D-Bus 专有接口 |
| `org.freedesktop.systemd1.Socket` | `.socket` 套接字单元 | ⚠️ 已注册 | 属性 9 个：`Result` `NAccepted` `NConnections` `ControlPID` `XAttrEntryPoint` `XAttrListen` `XAttrAccept` `CPUSetPartition` `OOMRules`（socket_obj.rs）；System K worker 已实现 |
| `org.freedesktop.systemd1.Timer` | `.timer` 定时器单元 | ❌ 未注册 | System C worker 已实现（单调/日历调度）；`systemctl list-timers` 不可用 |
| `org.freedesktop.systemd1.Path` | `.path` 路径监控单元 | ❌ 未注册 | System P worker 已实现（inotify） |
| `org.freedesktop.systemd1.Swap` | `.swap` 交换分区单元 | ❌ 未注册 | 解析支持，但**无对应 worker** |
| `org.freedesktop.systemd1.Slice` | `.slice` 资源切片单元 | ⚠️ 已注册 | 属性 2 个：`CPUSetPartition` `OOMRules`（slice_obj.rs）；System R worker 已实现 |
| `org.freedesktop.systemd1.Scope` | `.scope` 作用域单元 | ⚠️ 已注册（属性不可达） | 属性 4 个：`Controller` `TimeoutStopUSec` `RuntimeMaxUSec` `Result`；方法 `Abandon`（scope_obj.rs:74）；**⚠️ 自定义 Properties 分发（properties.rs）未处理 Scope 接口，`Get`/`GetAll(Scope)` 返回 `UnknownInterface`** |
| `org.freedesktop.systemd1.Device` | `.device` 设备单元 | ❌ 未注册 | System D worker 已实现（/dev + sysfs 发现、netlink） |

---

## 七、与前端工具的兼容性结论

| 前端操作 | 可用性 | 依赖接口 |
|---------|--------|---------|
| `systemctl start/stop/restart/reload/try-restart/reload-or-restart` | ✅ | StartUnit 等 + JobRemoved |
| `systemctl show` / `status`（部分信息） | ⚠️ | Unit 属性 69 个；时间戳全 0、无 journal 日志、部分属性缺失 |
| `systemctl set-property`（资源限制） | ✅ | SetUnitProperties |
| `systemctl reset-failed` / `daemon-reload` | ✅ | ResetFailed / Reload |
| `systemctl list-units / list-jobs / list-unit-files / is-enabled` | ⚠️ | 列表方法已实现；`list-jobs` 仅 Running |
| `systemctl cancel` | ❌ | CancelJob + Job 对象 |
| `systemctl enable/disable/mask/unmask/preset/link` | ❌ | 单元文件管理全家桶 |
| `systemctl set-default / get-default` | ❌ | Set/GetDefaultTarget |
| `systemctl set-environment / show-environment` | ❌ | SetEnvironment 等 |
| `systemctl kill / freeze / thaw / clean` | ❌ | KillUnit / FreezeUnit 等 |
| `systemctl reboot / poweroff / halt / kexec` | ❌ | System B 方法 |
| `systemctl list-timers` | ❌ | Timer 接口 |
| `systemd-analyze` | ❌ | Dump |
| `journalctl` | ❌ | 无 journal 后端（journald 未实现） |
| `GetUnitByPID` 类调用（logind 等） | ⚠️ | 命名不符（见全局问题 1） |