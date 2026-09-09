# System Alphabet 项目需求

你要实现一个全新的、跨平台的系统服务/资源管理器，项目叫 **System Alphabet**。用Rust实现

## 1. 项目目标

构建一个**编排与控制完全解耦**、**支持异构配置格式**、**跨平台**的系统服务编排中枢。它不再是 systemd 的简单垫片，而是一个通用的 **“服务依赖调度总线”**。通过适配不同的 Provider（解析器）和 Worker（执行驱动），它能使任何底层 init 系统（Systemd、SysV、OpenRC、Runit、Launchd）呈现出统一的 Systemd 语义前端，从而统一管理遗留系统与现代容器化工作负载。

## 2. 顶层架构

System Alphabet 不再是一个“仅解析 systemd Unit 的执行器”，而是一个“面向异构服务管理器的元控制平面（Meta-Control Plane）”。其核心哲学是：**解析（Parsing）、编排（Orchestration）与执行（Execution）三者完全脱钩。**

系统由以下三个逻辑平面构成，而非固定的物理进程：

### 2.1 解析适配层（Finder Layer）

这是系统最上层的“翻译官”。System Allocator 本身**不直接接触任何格式的配置文件**，而是依赖一组可插拔的 **Finder（查找器）** 来采集信息。

- **职责**：扫描不同路径（如 `/etc/systemd/system`、`/etc/init.d`、`/etc/service`），将 Systemd Unit、SysV LSB Headers、OpenRC runscript、Runit run 文件等**全部转化为统一的内部中间表示（IR）**。
- **核心价值**：通过这一层，System Allocator 得以“理解”所有历史遗留的启动脚本，而不需要为每种格式编写特殊的分支逻辑。

### 2.2 编排控制面（System Allocator —— 纯状态协调器）

System Allocator 是整个系统的“大脑”，但其职责被大幅精简与纯粹化。它**不负责执行，也不负责解析文本**，只做三件事：

- **动态依赖图维护**：接收 Finder 上报的 Unit 列表及依赖关系（如 `After`、`Requires`），构建一张全局的、版本化的有向无环图（DAG）。该图支持**运行时增量更新**（新增、删除节点无需重启 System Allocator）。
- **期望状态维护（Desired State）**：存储用户/系统期望的服务状态（例如 `nginx.service` 应为 `Running`），但绝不持有真实的 PID 或进程句柄。
- **拓扑决策与栅栏控制**：负责判定系统何时达到“稳态”（例如收到一个信号就表明所有System Finder都扫描完成，开始编排），并在依赖图发生变动时，仅对受影响的子图进行局部重算，而非全量排序。

**关键机制——动态拓扑**：
System A 的排序不再是一次性的“冷启动排序”。它具备**版本感知**能力。当 Provider 因延迟而晚汇报服务，或管理员在运行时通过 `systemctl start` 引入新依赖时，System A 会基于当前图版本进行**增量修正**，确保调度行为始终符合最新的依赖关系，同时不影响已经处于运行状态的无关服务。（类似systemctl daemon-reload）

### 2.3 异构执行驱动层（System Workers —— 抽象执行接口）

System Workers 不再是固定写死的 `System S`（Service）、`System M`（Mount）等进程，而是一个**面向接口的执行驱动（Execution Driver）**。

- **定义**：Worker 是一个抽象的执行单元，它只认 System A 下发的 **Task（任务）**，并不关心 Task 来自于哪种解析格式。
- **具体实现示例**：
  - **原生驱动（Native）**：System A 直接通过底层 API（fork/exec）管理进程（即原 System S 的逻辑）。
  - **代理驱动（Proxy）**：System A 不直接管理进程，而是调用外部工具（如 `/etc/init.d/nginx start`），委托给 SysV init 或 launchd 去实际执行。
  - **逻辑驱动（Logic）**：针对 `.target` 等特殊类型，Worker 仅做逻辑聚合判定（如检查所有依赖是否满足），不产生真实进程。
- **核心价值**：这种设计使得 **“用 Systemd 的依赖逻辑启动 SysV 服务”** 或 **“在 macOS 上用 launchd 执行 Systemd 格式的任务”** 成为可能——编排逻辑与底层运行时完全解耦。

## 3. 通信模型

内部通信完全基于一个轻量的 **IPC/事件总线**，不依赖 D-Bus。

- **传输层**：  
  - Linux: `AF_UNIX` + `SOCK_SEQPACKET`  
  - Windows: Named Pipe（暂时不考虑支持它）
- **序列化**：Protocol Buffers (protobuf) 或 MessagePack，紧凑且跨语言。
- **模式**：  
  - **RPC**：System A 向 System W 派发 Task，执行器回报 TaskResult。  
  - **Pub/Sub 事件流**：执行器(W)发布事件（如 `process.exit`），System A 订阅并响应。  
- **连接模型**：System A 作为服务器监听知名地址（如 `/run/systema/allocator.sock`），各 System W 启动后主动连接 SysA，注册自己的能力。（也就是”我处理哪一类unit“，例如 `"type": "service"/"target"/"mount"`），然后持续**拉取 (Pull)** 属于自己的 Task。连接是长连接、全双工。

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

## 5. 状态所有权严格分离（强化描述）

本项目最核心的不变量是 **“状态的绝对隔离”**：

- **System A（编排者）**：**仅持有“期望状态（Desired State）”**和**“依赖图结构（Graph Structure）”**。它既不知道服务的真实 PID，也不知道进程在哪个 CPU 核心上运行。它只关心“谁依赖于谁”以及“我期望谁活着”。
- **Provider（解析者）**：**仅持有“静态元数据（Static Metadata）”**。它只负责将磁盘上的文本翻译成结构化数据，不持有任何运行时状态。
- **Worker（执行者）**：**持有“运行时真相（Runtime Truth）”**。无论是原生进程、SysV 脚本还是 launchd 任务，只有 Worker 清楚某个服务此时此刻是否真正存活。Worker 通过事件流（ProcessExit、MountResult 等）异步反向同步给 System A，**System A 绝不能直接写入或推断 Worker 的内部状态机**。
