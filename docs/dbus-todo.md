# D-Bus 接口实现 TODO 列表

> 对照 [systemd D-Bus 接口文档](https://github.com/ChenPi11/systemd/tree/main/i-doc/system_bus) 整理。
>
> **状态说明：**
> - ✅ **完全实现**：入参、返回值、实际行为与 systemd 完全相同
> - ⚠️ **部分实现**：已有代码，但行为、返回值或参数处理与 systemd 不同（附说明）
> - ❌ **未实现**：接口/方法/属性不存在或返回空/错误

---

## 接口继承层级概览

```
org.freedesktop.systemd1.Manager         （路径：/org/freedesktop/systemd1）
org.freedesktop.systemd1.Unit            （路径：/org/freedesktop/systemd1/unit/<name>，基础接口）
    ├── org.freedesktop.systemd1.Service  （服务单元专有接口）
    ├── org.freedesktop.systemd1.Target   （目标单元专有接口，无额外方法/属性）
    ├── org.freedesktop.systemd1.Mount    （挂载单元）
    ├── org.freedesktop.systemd1.Automount（自动挂载单元）
    ├── org.freedesktop.systemd1.Socket   （套接字单元）
    ├── org.freedesktop.systemd1.Timer    （定时器单元）
    ├── org.freedesktop.systemd1.Path     （路径监控单元）
    ├── org.freedesktop.systemd1.Swap     （交换分区单元）
    ├── org.freedesktop.systemd1.Slice    （资源切片单元）
    ├── org.freedesktop.systemd1.Scope    （作用域单元）
    └── org.freedesktop.systemd1.Device   （设备单元）
org.freedesktop.systemd1.Job             （路径：/org/freedesktop/systemd1/job/<id>）
```

---

## 一、`org.freedesktop.systemd1.Manager`

对象路径：`/org/freedesktop/systemd1`
实现文件：`crates/system-a/src/dbus/manager.rs`

### 1.1 方法（Methods）

#### 单元查询

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `GetUnit(name)` | ⚠️ 部分实现 | 只支持非空 name；systemd 支持传空字符串返回调用方所属单元，未实现 |
| `GetUnitByPID(pid)` | ❌ 未实现 | 按 PID 查找单元 |
| `GetUnitByInvocationID(id)` | ❌ 未实现 | 按调用 ID（128位UUID）查找单元 |
| `GetUnitByControlGroup(cgroup)` | ❌ 未实现 | 按 cgroup 路径查找单元 |
| `GetUnitByPIDFD(pidfd)` | ❌ 未实现 | 按 PIDFD 查找单元（Linux 5.3+） |
| `LoadUnit(name)` | ⚠️ 部分实现 | 功能正确，但使用 `spawn_blocking` 同步加载，无 polkit 权限检查 |

#### 单元操作

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `StartUnit(name, mode)` | ⚠️ 部分实现 | `mode` 参数被接收但完全忽略（仅实现 `replace` 语义）；无 polkit 权限检查 |
| `StartUnitWithFlags(name, mode, flags)` | ❌ 未实现 | 带 flags 标志位的启动方法 |
| `StartUnitReplace(old_unit, new_unit, mode)` | ❌ 未实现 | 替换等待中的旧单元任务 |
| `StopUnit(name, mode)` | ⚠️ 部分实现 | `mode` 参数被忽略；停止时不传播到依赖当前单元的上游单元；无权限检查 |
| `ReloadUnit(name, mode)` | ⚠️ 部分实现 | `mode` 参数被忽略；无权限检查 |
| `RestartUnit(name, mode)` | ⚠️ 部分实现 | `mode` 参数被忽略；无权限检查 |
| `TryRestartUnit(name, mode)` | ⚠️ 部分实现 | 仅检查 Active 状态；未运行时返回伪造的 `job_id=0` 路径而非空路径；无权限检查 |
| `ReloadOrRestartUnit(name, mode)` | ⚠️ 部分实现 | `mode` 参数被忽略；无权限检查 |
| `ReloadOrTryRestartUnit(name, mode)` | ❌ 未实现 | 支持重载则重载，否则仅在运行时才重启 |
| `EnqueueUnitJob(name, job_type, job_mode)` | ❌ 未实现 | 返回详细的受影响作业信息列表 |
| `KillUnit(name, whom, signal)` | ❌ 未实现 | 向单元进程发送指定信号 |
| `KillUnitSubgroup(name, whom, subgroup, signal)` | ❌ 未实现 | 向单元 cgroup 子组发送信号 |
| `QueueSignalUnit(name, whom, signal, value)` | ❌ 未实现 | 发送带附加值的实时信号（sigqueue） |
| `CleanUnit(name, mask)` | ❌ 未实现 | 清理运行时文件、日志、状态等资源 |
| `FreezeUnit(name)` | ❌ 未实现 | 冻结单元（暂停 cgroup 中所有进程） |
| `ThawUnit(name)` | ❌ 未实现 | 解冻单元 |
| `ResetFailedUnit(name)` | ⚠️ 部分实现 | 功能基本正确，但无权限检查；即使单元不存在也静默返回 OK |
| `SetUnitProperties(name, runtime, properties)` | ❌ 未实现 | 运行时动态修改单元属性 |
| `BindMountUnit(name, source, destination, read_only, mkdir)` | ❌ 未实现 | 向服务命名空间添加绑定挂载 |
| `MountImageUnit(name, source, destination, read_only, mkdir, options)` | ❌ 未实现 | 向服务命名空间挂载磁盘镜像 |
| `RefUnit(name)` | ❌ 未实现 | 增加单元引用计数 |
| `UnrefUnit(name)` | ❌ 未实现 | 减少单元引用计数 |
| `StartTransientUnit(name, mode, properties, aux)` | ❌ 未实现 | 动态创建并启动临时单元 |

#### 进程与作用域

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `GetUnitProcesses(name)` | ❌ 未实现 | 获取单元中所有进程的 PID 列表 |
| `AttachProcessesToUnit(name, cgroup_path, pids)` | ❌ 未实现 | 将进程附加到单元的 cgroup |
| `RemoveSubgroupFromUnit(name, subgroup)` | ❌ 未实现 | 移除单元的 cgroup 子组 |
| `AbandonScope(name)` | ❌ 未实现 | 放弃 scope 单元 |

#### 作业管理

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `GetJob(id)` | ❌ 未实现 | 获取指定作业的 D-Bus 对象路径 |
| `GetJobAfter(id)` | ❌ 未实现 | 获取在此作业之后运行的作业列表 |
| `GetJobBefore(id)` | ❌ 未实现 | 获取在此作业之前必须完成的作业列表 |
| `CancelJob(id)` | ❌ 未实现 | 取消等待中的作业 |
| `ClearJobs()` | ❌ 未实现 | 清除所有等待中的作业 |

#### 系统管理

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `ResetFailed()` | ⚠️ 部分实现 | 功能基本正确，无权限检查 |
| `SetShowStatus(mode)` | ❌ 未实现 | 动态修改状态显示模式 |
| `Reload()` | ⚠️ 部分实现 | 仅重新扫描加载单元文件，不重新执行自身（`Reexecute`），异步执行无错误回报 |
| `Reexecute()` | ❌ 未实现 | 重新执行守护进程自身（热重启） |
| `Exit()` | ❌ 未实现 | 退出 systemd（仅限用户实例） |
| `Reboot()` | ❌ 未实现 | 系统重启 |
| `SoftReboot()` | ❌ 未实现 | 软重启（userspace reboot） |
| `PowerOff()` | ❌ 未实现 | 关机断电 |
| `Halt()` | ❌ 未实现 | 停止系统（不断电） |
| `KExec()` | ❌ 未实现 | kexec 重启 |
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
| `ListUnits()` | ⚠️ 部分实现 | 返回格式正确，但仅包含已加载单元，不包含已知但未加载的单元；`following` 字段固定为空 |
| `ListUnitsFiltered(states)` | ❌ 未实现 | 按状态过滤列出单元 |
| `ListUnitsByPatterns(states, patterns)` | ❌ 未实现 | 按状态和通配符模式列出单元 |
| `ListUnitsByNames(names)` | ❌ 未实现 | 按名称列出单元（包括未加载的） |
| `ListJobs()` | ⚠️ 部分实现 | 只返回 Running/Waiting 状态的作业；格式基本正确 |

#### 订阅与转储

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `Subscribe()` | ❌ 未实现 | 订阅单元状态变更信号 |
| `Unsubscribe()` | ❌ 未实现 | 取消订阅 |
| `Dump()` | ❌ 未实现 | 返回系统状态的文本转储 |
| `DumpUnitsMatchingPatterns(patterns)` | ❌ 未实现 | 按模式转储单元状态 |
| `DumpByFileDescriptor(fd)` | ❌ 未实现 | 通过 FD 输出转储 |
| `DumpUnitsMatchingPatternsByFileDescriptor(patterns, fd)` | ❌ 未实现 | 按模式通过 FD 输出转储 |

#### 单元文件管理

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `ListUnitFiles()` | ⚠️ 部分实现 | 只列出已加载进内存的单元，不扫描磁盘所有单元文件；路径硬编码为 `/usr/lib/systemd/system/`；`enabled`/`disabled` 判断仅基于 `WantedBy` 字段 |
| `ListUnitFilesByPatterns(states, patterns)` | ❌ 未实现 | 按状态和模式列出单元文件 |
| `GetUnitFileState(name)` | ❌ 未实现 | 获取单元文件的启用状态 |
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

---

### 1.2 属性（Properties）

#### 系统信息属性

| 属性名 | 状态 | 说明 |
|--------|------|------|
| `Version` | ⚠️ 部分实现 | 固定返回 `"255"`，不反映真实版本 |
| `Features` | ⚠️ 部分实现 | 固定返回空字符串，systemd 中为编译特性列表 |
| `Virtualization` | ⚠️ 部分实现 | 固定返回空字符串，未检测虚拟化环境 |
| `Architecture` | ✅ 完全实现 | 通过 `std::env::consts::ARCH` 返回正确架构名 |
| `Tainted` | ⚠️ 部分实现 | 固定返回空字符串 |
| `ManagerState` | ❌ 未实现 | 系统管理器状态（`initializing`/`starting`/`running`/`degraded`/`maintenance`/`stopping`） |

#### 启动时间戳属性

以下属性均已注册，但全部固定返回 `0`（未接入实际时间测量）：

| 属性名 | 状态 |
|--------|------|
| `FirmwareTimestamp` / `FirmwareTimestampMonotonic` | ⚠️ 部分实现（固定0） |
| `LoaderTimestamp` / `LoaderTimestampMonotonic` | ⚠️ 部分实现（固定0） |
| `KernelTimestamp` / `KernelTimestampMonotonic` | ⚠️ 部分实现（固定0） |
| `InitRDTimestamp` / `InitRDTimestampMonotonic` | ⚠️ 部分实现（固定0） |
| `UserspaceTimestamp` / `UserspaceTimestampMonotonic` | ⚠️ 部分实现（固定0） |
| `FinishTimestamp` / `FinishTimestampMonotonic` | ⚠️ 部分实现（固定0） |
| `SecurityStartTimestamp` / `SecurityStartTimestampMonotonic` | ⚠️ 部分实现（固定0） |
| `SecurityFinishTimestamp` / `SecurityFinishTimestampMonotonic` | ⚠️ 部分实现（固定0） |
| `GeneratorsStartTimestamp` / `GeneratorsStartTimestampMonotonic` | ⚠️ 部分实现（固定0） |
| `GeneratorsFinishTimestamp` / `GeneratorsFinishTimestampMonotonic` | ⚠️ 部分实现（固定0） |
| `UnitsLoadStartTimestamp` / `UnitsLoadStartTimestampMonotonic` | ⚠️ 部分实现（固定0） |
| `UnitsLoadFinishTimestamp` / `UnitsLoadFinishTimestampMonotonic` | ⚠️ 部分实现（固定0） |
| `InitRDGeneratorsStartTimestamp` / `...Monotonic` | ❌ 未实现 |
| `InitRDGeneratorsFinishTimestamp` / `...Monotonic` | ❌ 未实现 |
| `InitRDUnitsLoadStartTimestamp` / `...Monotonic` | ❌ 未实现 |
| `InitRDUnitsLoadFinishTimestamp` / `...Monotonic` | ❌ 未实现 |

#### 日志控制属性

| 属性名 | 状态 | 说明 |
|--------|------|------|
| `LogLevel` | ⚠️ 部分实现 | 固定返回 `"info"`，不可动态设置 |
| `LogTarget` | ⚠️ 部分实现 | 固定返回 `"journal"`，不可动态设置 |
| `LogColor` | ❌ 未实现 | 日志颜色开关 |
| `LogLocation` | ❌ 未实现 | 日志位置信息开关 |
| `LogTime` | ❌ 未实现 | 日志时间显示开关 |

#### 统计属性

| 属性名 | 状态 | 说明 |
|--------|------|------|
| `NNames` | ✅ 完全实现 | 实际返回已加载单元数量 |
| `NFailedUnits` | ✅ 完全实现 | 实际统计 Failed 状态单元数 |
| `NJobs` | ✅ 完全实现 | 实际统计 Running/Waiting 作业数 |
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
| `DefaultStandardOutput` | ⚠️ 部分实现 | 固定返回 `"journal"` |
| `DefaultStandardError` | ⚠️ 部分实现 | 固定返回 `"journal"` |
| `ControlGroup` | ⚠️ 部分实现 | 固定返回空字符串，未接入 cgroup 管理 |
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
| `DefaultTimerAccuracyUSec` | ⚠️ 部分实现 | 固定返回默认值，不可配置 |
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
| `DefaultLimitCPU` / `DefaultLimitCPUSoft` | ⚠️ 部分实现（仅 CPU，固定 u64::MAX） |
| `DefaultLimitFSIZE` / `...Soft` | ❌ 未实现 |
| `DefaultLimitDATA` / `...Soft` | ❌ 未实现 |
| `DefaultLimitSTACK` / `...Soft` | ❌ 未实现 |
| `DefaultLimitCORE` / `...Soft` | ❌ 未实现 |
| `DefaultLimitRSS` / `...Soft` | ❌ 未实现 |
| `DefaultLimitNOFILE` / `...Soft` | ❌ 未实现 |
| `DefaultLimitAS` / `...Soft` | ❌ 未实现 |
| `DefaultLimitNPROC` / `...Soft` | ❌ 未实现 |
| `DefaultLimitMEMLOCK` / `...Soft` | ❌ 未实现 |
| `DefaultLimitLOCKS` / `...Soft` | ❌ 未实现 |
| `DefaultLimitSIGPENDING` / `...Soft` | ❌ 未实现 |
| `DefaultLimitMSGQUEUE` / `...Soft` | ❌ 未实现 |
| `DefaultLimitNICE` / `...Soft` | ❌ 未实现 |
| `DefaultLimitRTPRIO` / `...Soft` | ❌ 未实现 |
| `DefaultLimitRTTIME` / `...Soft` | ❌ 未实现 |

#### 默认计账属性

| 属性名 | 状态 | 说明 |
|--------|------|------|
| `DefaultCPUAccounting` | ⚠️ 部分实现 | 固定返回 `false` |
| `DefaultBlockIOAccounting` | ⚠️ 部分实现 | 固定返回 `false` |
| `DefaultMemoryAccounting` | ⚠️ 部分实现 | 固定返回 `true` |
| `DefaultTasksAccounting` | ⚠️ 部分实现 | 固定返回 `true` |
| `DefaultIOAccounting` | ❌ 未实现 |  |
| `DefaultIPAccounting` | ❌ 未实现 |  |

#### 内存压力属性

| 属性名 | 状态 |
|--------|------|
| `DefaultMemoryPressureThresholdUSec` | ❌ 未实现 |
| `DefaultMemoryPressureWatch` | ❌ 未实现 |

---

### 1.3 信号（Signals）

| 信号名 | 状态 | 说明 |
|--------|------|------|
| `UnitNew(id, unit)` | ❌ 未实现 | 单元加载时发出 |
| `UnitRemoved(id, unit)` | ❌ 未实现 | 单元卸载时发出 |
| `JobNew(id, job, unit)` | ❌ 未实现 | 作业创建时发出 |
| `JobRemoved(id, job, unit, result)` | ⚠️ 部分实现 | 已在作业完成时发出；但 `Job` 对象本身未注册，且无 `JobNew` |
| `StartupFinished(firmware, loader, kernel, initrd, userspace, total)` | ❌ 未实现 | 系统启动完成时发出 |
| `UnitFilesChanged()` | ❌ 未实现 | 单元文件变更时发出 |
| `Reloading(active)` | ❌ 未实现 | 重载开始/完成时发出 |

---

## 二、`org.freedesktop.systemd1.Unit`

对象路径：`/org/freedesktop/systemd1/unit/<escaped_name>`
实现文件：`crates/system-a/src/dbus/unit_obj.rs`

> **当前状态：部分实现。** 单元对象已动态注册，`GetUnit`/`LoadUnit` 返回路径在 D-Bus 上可访问；但仅实现了少量方法与属性，且大量字段为固定值或空值。

### 2.1 方法（Methods）

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `Start(mode)` | ❌ 未实现 | 启动单元（Unit 接口级别） |
| `Stop(mode)` | ❌ 未实现 | 停止单元 |
| `Reload(mode)` | ❌ 未实现 | 重载单元 |
| `Restart(mode)` | ❌ 未实现 | 重启单元 |
| `TryRestart(mode)` | ❌ 未实现 | 尝试重启 |
| `ReloadOrRestart(mode)` | ❌ 未实现 | 重载或重启 |
| `ReloadOrTryRestart(mode)` | ❌ 未实现 | 重载或尝试重启 |
| `EnqueueJob(job_type, job_mode)` | ❌ 未实现 | 添加作业并返回详情 |
| `Kill(whom, signal)` | ❌ 未实现 | 向单元进程发送信号 |
| `KillSubgroup(whom, subgroup, signal)` | ❌ 未实现 | 向 cgroup 子组发送信号 |
| `QueueSignal(whom, signal, value)` | ❌ 未实现 | 发送带值的实时信号 |
| `ResetFailed()` | ⚠️ 部分实现 | 可将 `failed` 状态重置为 `inactive/dead`，但行为未完全对齐 systemd |
| `SetProperties(runtime, properties)` | ❌ 未实现 | 动态设置属性 |
| `Ref()` | ❌ 未实现 | 增加引用计数 |
| `Unref()` | ❌ 未实现 | 减少引用计数 |
| `Clean(mask)` | ❌ 未实现 | 清理资源 |
| `Freeze()` | ❌ 未实现 | 冻结 |
| `Thaw()` | ❌ 未实现 | 解冻 |
| `GetProcesses()` | ❌ 未实现 | 获取进程列表 |
| `AttachProcesses(cgroup_path, pids)` | ❌ 未实现 | 附加进程 |
| `RemoveSubgroup(subgroup)` | ❌ 未实现 | 移除 cgroup 子组 |

### 2.2 属性（Properties）

已实现（部分）属性（存在但部分为固定值/空值）：

| 属性名 | 状态 |
|--------|------|
| `Id` / `Names` / `Description` / `Following` | ⚠️ 部分实现 |
| `LoadState` / `ActiveState` / `SubState` | ⚠️ 部分实现 |
| `Requires` / `Wants` / `After` / `Before` | ⚠️ 部分实现 |
| `FragmentPath` / `SourcePath` | ⚠️ 部分实现 |
| `UnitFileState` / `UnitFilePreset` | ⚠️ 部分实现 |
| `Job` | ⚠️ 部分实现 |
| `CanStart` / `CanStop` / `CanReload` / `CanIsolate` / `CanFreeze` | ⚠️ 部分实现 |
| `NeedDaemonReload` / `JobTimeoutUSec` / `JobRunningTimeoutUSec` | ⚠️ 部分实现 |
| `ConditionResult` / `AssertResult` / `ActivationDetails` / `Refs` / `Transient` / `Perpetual` | ⚠️ 部分实现 |

主要仍未实现属性（非完整列表）：

| 属性名 | 状态 |
|--------|------|
| 依赖反向关系与传播类（`RequiredBy`、`RequisiteOf`、`PropagatesStopTo` 等） | ❌ 未实现 |
| 时间戳类（`StateChangeTimestamp`、`ActiveEnterTimestamp` 等） | ❌ 未实现 |
| 作业策略类（`JobTimeoutAction`、`OnSuccessJobMode`、`OnFailureJobMode` 等） | ❌ 未实现 |
| 启停限制类（`StartLimitIntervalUSec`、`StartLimitBurst`、`StartLimitAction`） | ❌ 未实现 |
| 条件明细类（`ConditionTimestamp`、`Conditions`、`Asserts`） | ❌ 未实现 |
| 其余 cgroup / SELinux / CollectMode / InvocationID 等高级属性 | ❌ 未实现 |

### 2.3 信号（Signals）

Unit 接口本身无额外信号，属性变更通知通过标准 `PropertiesChanged` 信号发出：

| 信号名 | 状态 |
|--------|------|
| `PropertiesChanged`（来自 `org.freedesktop.DBus.Properties`） | ⚠️ 部分实现 |

---

## 三、`org.freedesktop.systemd1.Service`（继承自 Unit）

对象路径：`/org/freedesktop/systemd1/unit/<service_name>`

> **当前状态：部分实现。** 服务单元路径已注册 `org.freedesktop.systemd1.Service` 接口，但仅覆盖少量属性。

### 3.1 方法（Methods）

| 方法名 | 状态 | 说明 |
|--------|------|------|
| `BindMount(source, destination, read_only, mkdir)` | ❌ 未实现 | 动态绑定挂载到服务命名空间 |
| `MountImage(source, destination, read_only, mkdir, options)` | ❌ 未实现 | 动态挂载镜像到服务命名空间 |
| `DumpFileDescriptorStore()` | ❌ 未实现 | 转储 FD 存储内容 |

以及继承自 `Unit` 的方法（大多未实现，见上节）。

### 3.2 属性（Properties，Service 专有）

| 属性名 | 状态 |
|--------|------|
| `Type` | ⚠️ 部分实现 |
| `ExitType` | ❌ 未实现 |
| `Restart` | ⚠️ 部分实现 |
| `RestartMode` | ❌ 未实现 |
| `RemainAfterExit` | ❌ 未实现 |
| `GuessMainPID` | ❌ 未实现 |
| `RootDirectoryStartOnly` | ❌ 未实现 |
| `OOMPolicy` | ❌ 未实现 |
| `PIDFile` | ❌ 未实现 |
| `BusName` | ⚠️ 部分实现 |
| `NotifyAccess` | ⚠️ 部分实现 |
| `MainPID` | ⚠️ 部分实现 |
| `ControlPID` | ⚠️ 部分实现 |
| `UID` / `GID` | ❌ 未实现 |
| `TimeoutStartUSec` / `TimeoutStopUSec` / `TimeoutAbortUSec` | ❌ 未实现 |
| `TimeoutStartFailureMode` / `TimeoutStopFailureMode` | ❌ 未实现 |
| `RuntimeMaxUSec` / `RuntimeRandomizedExtraUSec` | ❌ 未实现 |
| `WatchdogUSec` | ❌ 未实现 |
| `RestartUSec` / `RestartSteps` / `RestartMaxDelayUSec` / `RestartUSecNext` | ⚠️ 部分实现 |
| `NRestarts` | ❌ 未实现 |
| `RestartPreventExitStatus` / `RestartForceExitStatus` / `SuccessExitStatus` | ❌ 未实现 |
| `StatusText` / `StatusErrno` / `StatusBusError` / `StatusVarlinkError` | ❌ 未实现 |
| `Result` / `ReloadResult` / `CleanResult` / `LiveMountResult` | ⚠️ 部分实现 |
| `FileDescriptorStoreMax` / `NFileDescriptorStore` / `FileDescriptorStorePreserve` | ❌ 未实现 |
| `ExecCondition` / `ExecStartPre` / `ExecStart` / `ExecStartPost` | ❌ 未实现 |
| `ExecReload` / `ExecStop` / `ExecStopPost` | ❌ 未实现 |
| `WorkingDirectory` / `RootDirectory` | ❌ 未实现 |
| `User` / `Group` / `SupplementaryGroups` | ❌ 未实现 |
| `Environment` / `EnvironmentFiles` | ❌ 未实现 |
| `OpenFile` / `ReloadSignal` | ❌ 未实现 |
| cgroup 上下文属性（CPUAccounting、MemoryMax 等） | ❌ 未实现 |
| 执行上下文属性（命名空间、安全、能力等，约 100+ 属性） | ❌ 未实现 |

---

## 四、`org.freedesktop.systemd1.Target`（继承自 Unit）

> **当前状态：部分实现。** Target 相关对象路径存在基础 `Unit` 接口能力，但 Target 专有接口未实现。
>
> 注：Target 单元会注册基础 `Unit` 对象；但 `org.freedesktop.systemd1.Target` 专有接口尚未单独注册。

---

## 五、`org.freedesktop.systemd1.Job`

对象路径：`/org/freedesktop/systemd1/job/<job_id>`

> **当前状态：整个接口完全未实现。** 作业的 D-Bus 对象未被动态注册。`ListJobs`、`StartUnit` 与 `JobRemoved` 信号中出现的作业路径仅为引用值，在 D-Bus 上不可解析为实际 `Job` 对象。

### 5.1 属性（Properties）

| 属性名 | 状态 |
|--------|------|
| `Id` | ❌ 未实现 |
| `Unit` | ❌ 未实现 |
| `JobType` | ❌ 未实现 |
| `State` | ❌ 未实现 |
| `ActivationDetails` | ❌ 未实现 |

### 5.2 方法（Methods）

| 方法名 | 状态 |
|--------|------|
| `Cancel()` | ❌ 未实现 |
| `GetAfter()` | ❌ 未实现 |
| `GetBefore()` | ❌ 未实现 |

---

## 六、其他单元类型接口（全部未实现）

以下接口对应的单元类型在 System Alphabet Phase 1 中尚未实现对应 Worker，且相应的 D-Bus 对象也未注册：

| 接口名 | 对应单元类型 | 状态 |
|--------|------------|------|
| `org.freedesktop.systemd1.Mount` | `.mount` 挂载单元 | ❌ 未实现（System M 未实现） |
| `org.freedesktop.systemd1.Automount` | `.automount` 自动挂载 | ❌ 未实现 |
| `org.freedesktop.systemd1.Socket` | `.socket` 套接字单元 | ❌ 未实现 |
| `org.freedesktop.systemd1.Timer` | `.timer` 定时器单元 | ❌ 未实现（System C 未实现） |
| `org.freedesktop.systemd1.Path` | `.path` 路径监控单元 | ❌ 未实现 |
| `org.freedesktop.systemd1.Swap` | `.swap` 交换分区单元 | ❌ 未实现 |
| `org.freedesktop.systemd1.Slice` | `.slice` 资源切片单元 | ❌ 未实现 |
| `org.freedesktop.systemd1.Scope` | `.scope` 作用域单元 | ❌ 未实现 |
| `org.freedesktop.systemd1.Device` | `.device` 设备单元 | ❌ 未实现 |
