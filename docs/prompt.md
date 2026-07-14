# System Alphabet 项目需求

你要实现一个全新的、跨平台的系统服务/资源管理器，项目叫 **System Alphabet**。用Rust实现

## 1. 项目目标

构建一个**控制面与执行面严格分离**、**事件驱动**、**跨平台**的 systemd 系统替代方案。它不是 systemd 的 fork，而是一个“非pid1”的systemd shim方案，让非systemd init的系统也能用上systemd的前端生态，它能完美支持systemd，最终能够管理服务、挂载、定时任务、目标（target）等一系列单元。

## 2. 顶层架构

系统由两类组件构成：

- **System Allocator (System A)**：全局控制面。负责任务计划、依赖解析、事务协调，并对外暴露兼容 systemd1 所有的的 D-Bus 接口（仅用于外部兼容，System Alphabet 内部不用 D-Bus）。
- **System Workers (System W)**：执行面。每个执行器只负责一个资源域，拥有该域的运行时状态，通过异步 IPC 接受 System A 的 Task 并上报事件。

具体的组件列表及职责：

| 组件 | 全称 | 职责 | 拥有状态 |
| ------ | ------ | ------ | ------ |
| **System A** | System Allocator | 解析 Unit 文件，构建依赖图，拓扑排序生成 Task，派发 Task，处理事件，维护期望状态（最核心） | 仅维护 **Desired State** |
| **System S** | System Service | 管理服务进程的生命周期：fork/exec、重启策略、cgroup/Job Object 限制、输出捕获、看门狗 | `ServiceState` 自动机 (DEAD, STARTING, RUNNING, FAILED ...) |
| **System M** | System Mount | 管理文件系统挂载、自动挂载 | 挂载点状态 |
| **System C** | System Cron | 管理定时器（单调/日历），触发定时任务 | 定时器列表、下一次触发时间 |
| **System B** | System Boot | 负责管理引导/电源任务，如重启、关机、休眠、睡眠 | 电源、引导 |

注：由于 Target 的特殊性，System T 不单独作为 Worker，而是内嵌在 System A 内部，负责 Target 的激活逻辑。

**关键设计定理：** System A 永远不知道服务的真实 PID 或是否真的挂掉，它负责规划和”发布招标“；各 System W”接单“完成任务。

## 3. 通信模型

内部通信完全基于一个轻量的 **IPC/事件总线**，不依赖 D-Bus。

- **传输层**：  
  - Linux: `AF_UNIX` + `SOCK_SEQPACKET`  
  - Windows: Named Pipe（暂时不考虑支持它）
- **序列化**：Protocol Buffers (protobuf) 或 MessagePack，紧凑且跨语言。
- **模式**：  
  - **RPC**：System A 向 System W 派发 Task，执行器回报 TaskResult。  
  - **Pub/Sub 事件流**：执行器(W)发布事件（如 `process.exit`），System A 订阅并响应。  
- **连接模型**：System A 作为服务器监听知名地址（如 `/run/system-alphabet/allocator.sock`），各 System W 启动后主动连接 SysA，注册自己的能力。（也就是”我处理哪一类unit“，例如 `"type": "service"/"target"/"mount"`），然后持续**拉取 (Pull)** 属于自己的 Task。连接是长连接、全双工。

协议框架定义：

```protobuf
message Envelope {
    uint64 request_id = 1;
    string source = 2;
    string target = 3;
    string method = 4;  // e.g. "task.dispatch", "event.publish"
    bytes payload = 5;
}
```

Task 与 Event 消息的具体结构在实现时按需定义。

## 4. 线程与并发模型

所有组件（System A 和各 System W）均采用**单线程异步事件循环**。在 Rust 中选用 **Tokio**，利用 `epoll`（Linux）或 IOCP（Windows）驱动。严禁为每个服务创建线程。

## 5. 状态所有权严格分离

- **System A**：只存储期望状态（例如 `desired_state[“nginx.service”] = ACTIVE`）。
- **System S**：维护服务真实状态机，如 `DEAD → START_PRE → STARTING → RUNNING → … → FAILED`。System A 只能通过 TaskResult 或事件得知状态变化，绝不能直接写入执行器的状态。

## 6. 平台抽象

必须提供统一的 `ProcessHandle`、`MountHandle` 等平台无关接口。

- **Linux 实现**：使用 `fork()`、`execve()`、`pidfd`、`cgroup v2`、`signalfd`。
- **Windows 实现**：使用 `CreateProcessW()`、Job Object、IOCP、Windows Service API。

## 7. Unit 文件格式

保持与 systemd 完全相同的配置格式。使用Rust中解析systemd配置的轮子

## 8. 开发路线图

按以下阶段实现，每阶段产出可编译运行的代码。

### Phase 1：核心调度与服务管理 [已完成]

- 实现 System A 的骨架：Unit Loader、依赖图构建、拓扑排序、Task 生成器。
- 实现 System S 骨架：状态机、进程启动/停止/监视（Linux 版本，用简单 fork/exec）。
- 实现两者之间的 IPC：System A 与 Sysyem S 通过 Unix Socket 通信，Task 派发与结果回报。
- 支持 `systemctl start/stop sshd.service`，能正确启动和停止一个简单服务。
- 实现 `target` 的基本激活（System T 可作为 System A 内部逻辑，暂不独立）。

### Phase 2：事件总线与故障恢复

- 实现事件总线：pub/sub 机制，System S 发布 `process.exit`，SysA 触发 restart。
- 实现重启策略（on-failure, always）。
- System A 能正确从崩溃中恢复：重读 Unit 文件，与各 System E 恢复连接并同步状态。

### Phase 3：挂载与定时任务

- 实现 System M 和 System C，以及对应的 `.mount`、`.timer` 单元解析。
- 实现自动挂载（按需挂载）逻辑。
- System C 支持单调时钟与日历定时器，可触发 service 启动。

### Phase 4：跨平台支持（Windows）

- 将所有 System E 的平台抽象层实现 Windows 后端。
- 至少在 Windows 上能启动/停止一个守护进程（模拟服务），并进行挂载管理。

## 9. AI 的工作方式

- 请首先输出你对整个架构的理解，确认没有歧义。
- 然后从 Phase 1 开始，逐步给出每个模块的详细设计和 Rust 代码骨架，包括 Cargo.toml 依赖、模块划分、关键类型定义和核心逻辑伪代码（或可直接编译的代码）。
- 每完成一个 Phase，等待我对当前阶段的测试反馈后再继续下一阶段。
- 代码风格遵循 Rust 社区惯例，注重错误处理和日志。
- 所有 IPC 消息使用 protobuf 定义，放在独立的 `proto/` 目录。

现在，请开始你的理解和 Phase 1 的设计方案。
