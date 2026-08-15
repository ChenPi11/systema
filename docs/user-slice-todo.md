# user-\<UID>.slice 按需物化 实现方案 TODO 列表

> 会话 scope(`session-<id>.scope`)的 `Slice=user-<UID>.slice` 依赖在 System A 中缺失,导致 `StartTransientUnit` 规划失败。本文档汇总当前实现方案的全部细节、logind 源码实证、协议设计与里程碑拆分。
>
> **状态说明：**
>
> - ✅ **已确认/已实现**：事实已核实或代码已存在
> - ⚠️ **待实现**：方案已定,尚未实现
> - ❌ **未实现**：缺口,已记录

---

## 一、背景与问题

### 1.1 会话 scope 创建流程(当前)

System A 已实现 `StartTransientUnit` / `StartTransientUnitMany` D-Bus 方法
(`crates/backend/systema-sysa/src/dbus/manager.rs:783` / `:811`),`transient_unit_from_properties`
(`manager.rs:115`) 将 `Slice=` 属性镜像为 `Requires=` + `After=` 边(`manager.rs:157-163`)。

`start_transient_unit_many` 的循环语义(`manager.rs:826-838`)：逐个
`insert_transient_unit` → `activate_transient_unit`;切片/非 scope 瞬态单元走快速路径
立即 active(`scheduler/mod.rs:108-156`,无 worker 往返);scope 走正常作业机制派发给
System E(注册 `scope` 类型,`systema-syse/src/ipc.rs:14`)。

### 1.2 问题

事务规划器对 `Requires=`/`BindsTo=` 引用缺失单元的行为是**硬错误**
(`crates/backend/systema-sysa/src/scheduler/transaction.rs:501-506`,`MATTERS` 标志;
`After=` 不会为缺失单元拉取作业、也不会失败——`transaction.rs:499-527` 只对
requires/binds_to/wants/upholds/requisite/conflicts 建作业)。

因此 `StartTransientUnit("session-1.scope", {Slice: "user-0.slice", PIDs: [...]})`
在 `user-0.slice` 未加载时必然失败。单元级测试已覆盖该行为
(`scheduler/mod.rs:2250-2311`:scope 无 worker 失败、scope 派发 worker、slice 快速路径)。

### 1.3 涉及的角色分工(现状,已重新审查)

| 组件 | 职责 | 关键位置 |
| ------ | ------ | ---------- |
| System A | 调度器,零单元类型语义;同时是 **finder server**(单元注入端点)与静态文件 loader | `scheduler/`, `dbus/`, `ipc/server.rs:194-195`, `unit/loader.rs` |
| System R (sysr) | `slice` 类型作业;cgroup 层级;`user.sessions` 驱动 slice 生命周期;**通过 finder API 直接向 Allocator 注入 slice 定义(不经 System F)** | `systema-sysr/systema-sysr/src/{ipc,register,worker}.rs` |
| System D (sysd) | `device` 类型作业;设备发现;**通过 finder API 直接注入 device 单元(不经 System F)** | `systema-sysd/src/{ipc,engine,controller}.rs` |
| System E (syse) | `scope` 类型作业;cgroup 监视/杀进程 | `systema-syse/src/{ipc,controller}.rs` |

### 1.4 finder API 与单元注入路径(事实审查)

**finder API 是 `libsysa::finder::UnitFinder` 客户端**(`crates/libsysa/src/finder.rs:9-107`),
直连 **System A** 的 `allocator.sock`(`finder.rs:16,25`),信封方法
`finder.register_units` / `finder.commit_units` / `staging.query`(`finder.rs:34,62,89`);
**server 端在 System A**(`ipc/server.rs:194-195` `handle_finder_register/commit`,
staging 状态在 `state.rs:399` `staging_areas`)。

**任何组件都可以用该 API 向 Allocator 创建单元**(信封 source 固定写 "system-f"
仅是客户端标签,`finder.rs:34`):

| 注入者 | 内容 | staging 名 | 位置 |
| -------- | ------ | ----------- | ------ |
| System F | 磁盘上已存在的静态单元(systemd 配置发现) | `systema-sysf/discovery` | `systema-sysf/src/main.rs:85-108` |
| System D | 动态 device 单元 | `systema-sysd/discovery` | `systema-sysd/src/engine.rs:234-237` |
| System R | 动态 slice 单元(`user.slice`/`user-<UID>.slice`) | `systema-sysr/slices` | `systema-sysr/.../register.rs:63-83` |

### 1.5 do-one-thing 符合性结论

- 现状**符合**原则:设备注入归 System D、slice 注入归 System R、磁盘静态加载归
  System F,三者均通过 finder API 直连 System A——没有"System F 替 D/R 加载
  动态单元"的设计。
- **约束(本文档立场)**:`user-<UID>.slice` 的生命周期**不得涉及 System F**——
  F 只允许加载磁盘中已存在的单元;slice 是动态单元,注入只允许走
  System R → finder API → System A。

---

## 二、调用方实证:真实 logind 的行为

> 结论先行:**logind 从不声明 slice**(aux 为空),`user-<UID>.slice` 的存在是 init
> 侧(PID1)按需物化的责任。因此"调用方声明 slice"(方案 1)对真实 logind 不成立。

### 2.1 `manager_start_scope`(systemd `src/login/logind-dbus.c:4437-4578`)

创建会话 scope 的实际 D-Bus 调用：

- 方法:**`StartTransientUnit`**(非 Many),mode = `"fail"`
- **aux 单元数组为空**(`"a(sa(sv))", 0`,第 4549 行)
- 属性清单:
  - `Slice=user-<UID>.slice`
  - `Description=`
  - `Requires=` + `After=` → `user-runtime-dir@<UID>.service`
  - `Wants=` + `After=` → `user@<UID>.service`
  - `After=` → `systemd-logind.service`, `systemd-user-sessions.service`
  - `RequiresMountsFor=` → 用户 home 目录
  - `SendSIGHUP=true`
  - `PIDs=` / `PPIDs=`(pidfd 失败时重试不带,见 4554-4572)
  - `OOMPolicy=continue`、`TasksMax=UINT64_MAX`(禁用,交由 slice 控制)

调用方: `session_start_scope`(`src/login/logind-session.c`,约 800 行处) →
`manager_start_scope`,传入 `requires=user-runtime-dir@UID.service`、
`wants=user@UID.service`。

### 2.2 `user_start`(systemd `src/login/logind-user.c`)

登录时先 `StartUnit` 启动 `user-runtime-dir@<UID>.service` 与 `user@<UID>.service`
(`user_start_runtime_dir` / `user_start_service_manager`),它们所在模板自带
`Slice=user-%i.slice`;`user-<UID>.slice` 因此被 PID1 拉入加载。
**logind 全程不把 slice 作为瞬态/aux 单元声明。**

另:`user_update_slice`(同文件)通过 **`SetUnitProperties`** 对 `user-<UID>.slice`
设置 `TasksMax`/`MemoryMax`/`MemoryHigh`/`CPUWeight`/`IOWeight`(best-effort,异步)。

### 2.3 PID1 侧物化机制(systemd `src/core/slice.c`)

- `slice_load`(新单元加载) → `slice_add_parent_slice`:`unit_add_dependency_by_name`
  (`UNIT_IN_SLICE` 隐式依赖 + **按名加载父 slice**),逐级物化父链直到 `-.slice`
- `slice_verify` 强制"必须位于父 slice 内",否则拒绝
- `-.slice`(root)与 `system.slice` 为 perpetual 单元,启动即合成
  (`slice_make_perpetual` / `slice_enumerate_perpetual`)

即 systemd 的机制 = **`Slice=` 解析时的按需加载 + slice 父链自动物化**,知识在
PID1 的类型化逻辑(slice.c)中,而非调用方。

### 2.4 对本项目的含义

- 方案 1(调用方在 `StartTransientUnitMany` 主列表/aux 里声明 slice):仅对自研
  会话管理器有效;真实 logind 不使用该形态 → **不采用作为主路径**
- 正确形态:init 侧按需物化缺失的 slice 单元,知识归属不与 logind 冲突的组件

---

## 三、方案演进与最终决策

### 3.1 曾评估的方案

| 方案 | 内容 | 结论 |
|------|------|------|
| 方案 1 | 调用方声明 slice(`StartTransientUnitMany` 先声明 `user-0.slice`) | ⚠️ 对自研调用方零改动有效;对真实 logind 不成立(logind aux 为空) |
| 方案 2 | 瞬态 `Slice=` 仅做 `After=` 边(统一指令调整) | ⚠️ 单调用可成功,但 slice 单元仍不在图中,`StartUnit(user-0.slice)`/`SetUnitProperties` 仍失败;偏离 systemd |
| 方案 3 | 通用 on-demand-load 钩子 + 定义来源 | ⚠️ 方向正确,但定义来源需归位:slice 是**运行时/动态**单元,定义知识归拥有其生命周期的 System R(经 finder API 注入),**不归 System F**(System F 仅静态磁盘加载,user-X.slice 生命周期不得涉及 sysf) |

### 3.2 最终决策(✅ 已确认)

**worker 平权:System R 合成 slice;System A 保持类型无感;**

- **动态加载归 System R**:System R 本就拥有 slice 生命周期与定义注入
  (`register.rs` 经 finder API 直连 System A 提交 + `user.sessions` 推送,
  `systema-sysa/src/ipc/server.rs:1390-1434`),按需拉取协议是补齐"plan 时缺失"
  的最后一块
- System Allocator 获得**类型无关**的通用机制:plan 前发现缺失单元 → 按其后缀类型询问
  对应 worker 能否提供定义 → 提交 → 重规划

与 systemd 的对应:systemd 中物化知识在 PID1(slice.c);本项目将同样的知识放到
"负责该单元类型的 worker"(System R),System A 只提供通用拉取协议——不违背
do-one-thing(每个 worker 拥有自己单元类型的定义)。

---

## 四、目标架构与数据流

### 4.1 协议设计:`unit.define`(✅ 已实现)

**System A 侧(通用钩子,零类型特判):**

1. **plan 前预扫描**:`enqueue_start_with_mode`(`scheduler/mod.rs:76`)、
   `enqueue_job_type`(`:204`)、`enqueue_job`(`:320`)均为 async 入口;在构建事务前
   对目标单元(含将展开的依赖边)收集"引用但缺失"的单元名
2. **静态加载优先**:对每个缺失名先走 System A 自己的 loader
   (`loader.rs` `ensure_loaded_from_disk`,含模板实例化)——磁盘上存在的单元
   (service/mount/模板实例等)由 A 按需加载,不进入协议(未决问题 2 定案)
3. **按类型查 worker(能力门控)**:对余下的**动态**缺失名,查 `state.workers`
   中注册了该后缀类型**且声明 `supports_unit_define`** 的 worker(协议第 3 条)——
   `slice` → System R;未声明能力的 worker 不会被询问(避免 60s 超时)
4. **发送 `unit.define` 信封**:沿用 request_id 应答机制(`ipc/server.rs:319`
   `method.result` 分发、`state.rs:320` `serial_completion_txs`),带超时;
   payload = 请求的单元名(可批量)
5. **提交并重试**:worker 回复 UnitIR(JSON,与 finder 提交同序列化格式
   `systema-sysf/src/ir.rs`)→ 走现有 finder 提交路径(server 端在 System A,
   `ipc/server.rs:194-195`;与 `register.rs` 的提交幂等 merge 同路,
   `sysr/register.rs:13-14`、`state.rs` `staging_areas`)→ 重新规划,
   **有界重试(1 次)**;仍缺失则保持现有硬错误语义

**System R 侧(`worker.rs` 新增 handler,知识全在 R):**

6. 对 `unit.define` 请求:对**任意合法 slice 名**合成定义并回复

### 4.2 全链路数据流(目标)

```
logind / 会话管理器
  └─ StartTransientUnit("session-1.scope", {Slice:"user-0.slice", PIDs:[...]}, aux=[])
       │
       ▼ System A
  预扫描: 发现 user-0.slice 缺失
  按类型查 worker("slice" → system-r-1)
  unit.define("user-0.slice") ───────────────► System R
       │                                         └─ 合成 UnitIR(user-0.slice → user.slice → root)
  提交定义(幂等 merge) ◄────────────────────────┘
  重规划:
    Start(user-0.slice)   → 派发 System R(建 cgroup,报 active)
    Start(session-1.scope)→ 派发 System E(挂 PIDs,监视 cgroup)
       │
  会话 scope active → user.sessions(uid, count) → System R(user slice 生命周期,推送幂等)
```

### 4.3 与现有推送机制的共存(✅ 已确认设计)

- System R 连接时的推送注册(`sysr/ipc.rs:67-68` `ensure_user_slice_root` +
  `spawn_user_slice_registration`)与 `user.sessions` 驱动的注册**保留不变**
- 拉取协议(`unit.define`)只补 plan 期缺口;两次路径都对 System A 的合并
  (merge over existing)幂等(`sysr/register.rs:13-14`)

---

## 五、System R 合成规则(✅ 已实现)

| 规则 | 说明 |
|------|------|
| 单元名合法性 | 仅接受合法 slice 名(形如 `*.slice`) |
| 父链 | 由名字计算:`user-0.slice → user.slice → root`;任意合法 slice 名通用合成(如 `foo.slice → root`),与 systemd 的 `slice_build_parent_slice` 语义对应 |
| 合成内容 | `UnitIR { id, unit_type: Slice, slice: 父slice, description, source_format: "dynamic" }`(参考 `sysr/register.rs:35-56` 的 `user_slice_ir`) |
| 工具 | 复用 `libsysa/src/unit_name.rs` 助手(`parse_user_slice_uid` 已存在)与 `systema-sysr-common::user_slice_name` |
| 拒绝 | 非 slice 名 → 明确拒绝(该协议不用于服务/挂载等静态单元,那些仍走磁盘加载/模板实例化) |

---

## 六、事务语义变化(✅ 已实现)

- 重规划后,`Requires=`/`Slice=` 拉入的 `Start(user-0.slice)` 作业与 scope 作业同事务
  调度——与 systemd 事务语义对齐(scope 启动连带 slice 启动)
- 快速路径保持:直接对瞬态 slice 的 `activate_transient_unit` 仍即时完成
  (`scheduler/mod.rs:108-156`),不受影响
- 无对应 worker 连接时的语义:保持现有硬错误("No worker available" 家族,
  `scheduler/mod.rs:375`、`manager.rs:371`),错误信息可注明"尝试 unit.define 未果"

---

## 七、logind D-Bus 接口面审计(✅ 已完成)

来源:logind 启动即 `Subscribe`(`src/login/logind.c` `manager_connect_bus`),
并匹配 `JobRemoved`/`UnitRemoved`/`PropertiesChanged`/`Reloading` 信号。

| 接口 | System A 现状 | 位置/说明 |
|------|--------------|-----------|
| `StartTransientUnit` / `Many` | ✅ | `dbus/manager.rs:783/811` |
| `Subscribe` / `Unsubscribe` | ✅ | `dbus/manager.rs:1082` |
| `JobRemoved` 信号 | ✅ | `scheduler/mod.rs:199`、`dbus/mod.rs:290-321` |
| `PropertiesChanged` | ✅ | `dbus/manager.rs:1078` |
| `AbandonScope` | ✅ | `dbus/manager.rs:1135` |
| `StartUnit` / `StopUnit` / `GetUnit` | ✅ | `dbus/manager.rs` |
| `SetUnitProperties` | ✅ | `dbus/manager.rs` `set_unit_properties`(仅 resource-control 指令,logind `user_update_slice` 的完整指令集已映射);更新状态 RC 后经事件总线推 `UnitResourceEvent` 给 System R |
| `GetUnitByPID` / `GetUnitByPIDFD` | ✅ | `dbus/manager.rs`;pidfd→PID 走 `/proc/self/fdinfo` 的 `Pid:` 行(无 pidfd_getpid 系统调用,glibc 2.36 库函数不依赖) |
| `UnitRemoved` / `Reloading` 信号 | ✅ | 信号+通道已就绪(`dbus/mod.rs`):`Reloading` 在 `reload()` 前后发出;`UnitRemoved` 通道已接信号任务,**暂无生产者**(当前无单元卸载路径,卸载机制单列后续项) |
| `KillUnit` | ❌ 审计结论:缺口 | logind `user_kill`/`session_kill` 使用;**全链路缺失**:无 D-Bus 方法、`UnitController` trait 无 `kill` 方法、worker 无信号处理。与物化机制无关,单列后续项(需扩展 trait + 信封协议) |

**结论**:本机制(slice 按需物化)之外的 logind 兼容缺口是
`SetUnitProperties`(必需)+ `GetUnitByPID(FD)` + `UnitRemoved`/`Reloading` 信号。三者均已实现(见上表)。

---

## 八、验证计划(⚠️ 待实现)

| 层级 | 内容 | 环境要求 |
|------|------|----------|
| 单元测试 | scheduler 测试:缺失单元 → 预扫描 → 定义提交 → 重规划成功;重试耗尽 → 硬错误保持 | `cargo test -p systema-sysa` |
| 协议集成测试 | `unit.define` 请求/应答编解码;System R 合成规则(合法/非法 slice 名) | `cargo test` |
| D-Bus 契约替身 | 扮演 logind 的调用方示例(examples/),按 2.1 的属性形态调用 `StartTransientUnit`,验证全链路 | root + 系统总线 |
| 真实 logind 冒烟 | 无 systemd 宿主环境运行真 logind;预检查:系统总线、`/run/systemd/private` 决策(sd-bus 在存在私有 socket 时优先走它) | 无 systemd 的主机/容器 |

---

## 九、里程碑拆分

| 里程碑 | 内容 | 状态 |
|--------|------|------|
| M0 | 本设计文档 | ✅ 完成 |
| M1 | 物化机制:预扫描 + `unit.define` 协议 + 提交重试(System A 通用钩子) + System R 合成 handler + 单元/集成测试 | ✅ 完成 |
| M2 | logind 接口面补齐:`SetUnitProperties`(必需)、`GetUnitByPID(FD)`、`UnitRemoved`/`Reloading` 信号、`KillUnit` 审计 | ✅ 完成 |
| M3 | 端到端:契约替身示例 + E2E 脚本;真实 logind 冒烟(环境允许时) | ⚠️ 待实现 |

---

## 十、未决问题

1. ~~`unit.define` 的请求粒度假定:单名 vs 批量;超时值与重试策略~~ **已定**:按 worker 类型批量;`UNIT_DEFINE_TIMEOUT = 60s`(与作业超时同量级),plan 后 `UnitNotFound` 有界重试 1 次
2. ~~模板实例(如 `user-runtime-dir@<UID>.service`)是否也走该协议~~ **已定**:否。
   预扫描先走 System A 静态加载器(`loader.rs` `ensure_loaded_from_disk`,按需加载 +
   模板实例化,同 `load_unit_flexible:547`);只有磁盘上不存在的**动态**单元才进入
   `unit.define`
3. ~~`unit.define` 对非 slice 类型的通用性~~ **已定**:协议类型无关,但只有**声明
   `WorkerRegistration.supports_unit_define`** 的 worker 会被询问(注册能力标志,
   `state.rs` `WorkerEntry.supports_unit_define`)。当前仅 System R 声明;其余 worker
   不实现该协议,也就永远不会收到请求(修复:旧行为会把缺失的 service/scope/device
   发给其 worker,后者警告"Unexpected method"并沉默 → 60s 超时)
4. ~~`SetUnitProperties` 的资源限制属性如何落到 System R 的 cgroup 控制~~ **已定**:
   `manager.rs` 更新状态 `ResourceControl` → `push_resource_update` 合成 `UnitStateChange` 事件
   → `WorkerEventForwarder` 重投影为 `UnitResourceEvent` → System R `handle_resource_event` 应用
   (`sysr/worker.rs:138`)——与状态迁移推送同路,幂等

---

## 附录:关键代码位置索引

**本项目(Systema):**

| 位置 | 内容 |
|------|------|
| `crates/libsysa/src/finder.rs:9-107` | `UnitFinder` 客户端:直连 System A(allocator.sock);`finder.register_units`/`finder.commit_units`/`staging.query`;**任何组件可用** |
| `crates/backend/systema-sysa/src/ipc/server.rs:194-195,763,829` | finder server 端点(`handle_finder_register/commit`、`finder.ack`) |
| `crates/backend/systema-sysa/src/state.rs:399` | `staging_areas`(UID 绑定 staging 状态) |
| `crates/backend/systema-sysf/src/main.rs:85-126` | System F = 静态加载器:SystemdFinder 发现磁盘单元 → 经 UnitFinder 注入(`systema-sysf/discovery`) |
| `crates/backend/systema-sysf/src/lib.rs:10-37` | `Finder` trait:仅"从特定 init 系统的配置文件发现/解析",无动态单元逻辑 |
| `crates/backend/systema-sysd/src/engine.rs:234-237` | System D 经 UnitFinder 注入 device(`systema-sysd/discovery`) |
| `crates/backend/systema-sysa/src/unit/mod.rs:8-9` | System A ↔ sysf 关系 = 仅 UnitIR 类型与 systemd parser 共享 |
| `crates/backend/systema-sysa/src/dbus/manager.rs:115-197` | `transient_unit_from_properties`(Slice= → Requires+After 镜像) |
| `crates/backend/systema-sysa/src/dbus/manager.rs:783-839` | `StartTransientUnit(Many)` |
| `crates/backend/systema-sysa/src/scheduler/mod.rs:84-156` | `activate_transient_unit`(scope 派发 / slice 快速路径) |
| `crates/backend/systema-sysa/src/scheduler/transaction.rs:499-527` | 依赖边 → 作业;Requires=MATTERS、After=不拉作业 |
| `crates/backend/systema-sysa/src/scheduler/mod.rs:709-734` | worker 按 unit_type 查找/派发 |
| `crates/backend/systema-sysa/src/ipc/server.rs:319` | `method.result` 分发 |
| `crates/libsysa/src/worker_ipc.rs` | `WorkerIpc`(连接/注册/派发);`WorkerRegistration.supports_unit_define` 能力声明(`WorkerIpc::supports_unit_define()`) |
| `crates/backend/systema-sysa/src/unit/loader.rs` | `ensure_loaded_from_disk`(预扫描静态加载优先,含模板实例化) |
| `crates/backend/systema-sysa/src/scheduler/mod.rs:381` | `request_unit_definition`(静态加载优先 + 能力门控 + 批量/超时/有界重试) |
| `crates/backend/systema-sysa/src/ipc/server.rs:1390-1434` | `user.sessions` → System R |
| `crates/backend/systema-sysr/systema-sysr/src/register.rs:35-83` | `user_slice_ir` / `commit_slice`(经 finder API 直连 System A,幂等) |
| `crates/backend/systema-sysr/systema-sysr/src/ipc.rs:30-68` | WORKER_UNIT_TYPES=["slice"]、订阅/注册 |
| `crates/backend/systema-syse/src/ipc.rs:14` | WORKER_UNIT_TYPES=["scope"] |
| `crates/backend/systema-sysa/src/unit/loader.rs:90` | `load_units_matching`(按模式按需加载) |
| `crates/backend/systema-sysa/src/unit/loader.rs:254-263` | `load_unit_flexible`:模板文件回溯实例化(`foo@.service`);裸模板拒绝加载 |
| `crates/libsysa/src/unit_name.rs:202` | `parse_user_slice_uid` |

**systemd 源码(实证):**
位置：/home/chenpi11/Projects/systema/systemd

| 位置 | 内容 |
|------|------|
| `src/login/logind-dbus.c:4437-4578` | `manager_start_scope`:StartTransientUnit、aux 空、属性清单 |
| `src/login/logind-session.c`(`session_start_scope`) | requires/wants/extra_after 参数来源 |
| `src/login/logind-user.c`(`user_start`/`user_update_slice`) | StartUnit user-runtime-dir@ / user@;SetUnitProperties 设 slice 限制 |
| `src/core/slice.c`(`slice_load`/`slice_add_parent_slice`/`slice_verify`) | 父链按需物化、IN_SLICE 隐式依赖 |
| `src/login/logind.c`(`manager_connect_bus`) | Subscribe + JobRemoved/UnitRemoved/PropertiesChanged/Reloading |
