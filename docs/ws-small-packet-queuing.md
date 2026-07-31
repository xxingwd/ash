# WebSocket / TCP 小包排队问题分析

## 问题现象

当 WebSocket 连接频繁发送小尺寸帧（控制帧、JSON-RPC 通知、心跳等）时，数据包不会立刻发送出去，而是排队等待一段时间（典型 40ms–200ms）才到达对端。这种延迟在交互频繁但单包数据量小的场景下尤为明显。

## 根本原因

### 1. Nagle 算法（RFC 896）

TCP 实现默认启用 Nagle 算法，其核心逻辑是：

```
if 已发送数据 < 拥塞窗口 && 未收到上次数据的 ACK:
    缓存当前数据，等待
else:
    立即发送
```

Nagle 的目的**不是"攒批"**，而是防止大量小包淹没网络。但对于 WebSocket 这类需要低时延的协议，它带来了副作用：

- WebSocket 帧头很小（2–14 bytes），Payload 可能只有几十字节
- Nagle 会等收到上一个包的 ACK 再发送下一个包
- 在长肥网络中 RTT 可能达到 50–200ms，此时小包被逐一延迟

### 2. Delayed ACK（RFC 1122）

TCP 接收端延迟确认（通常最多 200ms），期待合并 ACK 或捎带在响应数据中。

当发送端 Nagle 等待 ACK、接收端 Delayed ACK 等待数据时，两者形成**死锁**：

```
发送端                           接收端
  │                                │
  ├─ 发送小包 (len=50) ──────────► │  (启动 delayed ACK 定时器)
  │  (Nagle: 等待 ACK)             │  (等待更多数据再发 ACK)
  │                                │
  │  ... 200ms 之后 ...            │
  │ (ACK 超时到达)                 │
  │◄──────── ACK ──────────────── │
  ├─ 发送下一个小包 ──────────────► │
```

### 3. 补充因素

| 因素 | 说明 |
|------|------|
| **拥塞窗口初始值 (initcwnd)** | 默认 10 MSS (~14KB)，小包场景不受限 |
| **HTTP/2 HOL blocking** | 单个 HTTP/2 连接上的队头阻塞（头压缩 + 流复用），在 HTTP/2 的 TCP 层仍存在 |
| **WebSocket 帧重组** | WebSocket 库可能在用户态攒片，和 Nagle 叠加 |
| **操作系统网络栈** | Linux `tcp_slow_start_after_idle` 默认开启，空闲后连接重新慢启动 |
| **Cork / TCP_CORK** | 主动设置的 TCP_CORK 会强制等待，不设置 Nagle 仍生效 |

## Ash 项目中的排查路径

### 场景 1：LLM API SSE 流（ash-protocol）

Ash 使用 `reqwest` + `reqwest_eventsource` 从 LLM API 消费 SSE 事件。

```rust
// crates/ash-protocol/src/sse.rs
let mut source = request.eventsource()?;
```

`reqwest` 默认的 HTTP/1.1 连接会在底层启用 `TCP_NODELAY`，因此**这个路径通常不受 Nagle 影响**。但如果 provider 返回的小帧过于频繁（如 Anthropic 的 `input_json_delta`），**接收端**的 recv buffer 可能会等更多数据才传递给应用层，这里表现为 `read()` 返回延迟。

**排查要点：**

- 检查 `reqwest::Client::builder().tcp_nodelay(true)` 是否显式设置（默认是 true）
- 检查是否意外使用了 HTTP/2（HTTP/2 的 TCP 层仍需 `TCP_NODELAY`）

### 场景 2：MCP 子进程通信（ash-agent/mcp）

MCP 通过 `TokioChildProcess` 与子进程用 stdio 交换 JSON-RPC 消息：

```rust
// crates/ash-agent/src/mcp.rs
let transport = TokioChildProcess::new(&mut cmd)?;
```

子进程管道（pipe）**不走 TCP 协议栈**，不受 Nagle 影响。但 JSON-RPC 消息有另一个问题：

- `rmcp` 使用 `tokio_util::codec::FramedRead` 按换行符 `\n` 切分消息（见 `io.rs` 的 `JsonRpcMessageCodec`）
- 如果子进程连续写入大量小 JSON 行，OS 管道 buffer（通常 64KB）会延迟向读端推送数据
- `FramedRead` 每次 `buf_read` 读到 buffer 满或读到换行符才返回

**排查要点：**

- 管道延迟通常 < 10ms，不会出现 200ms 级别延迟
- 如果出现长延迟，检查子进程 stdout 是否 flush（典型的：Python `print(..., flush=True)`、Node `process.stdout.write()`）
- 确认 `TokioChildProcess` 内部没有额外 buffering

### 场景 3：直连 WebSocket（当前未使用）

Ash 目前不使用 `tokio_tungstenite` 等原生 WebSocket 库。如果后续接入 WebSocket 协议（如 OpenAI Realtime API），需要注意：

```rust
// 伪代码 — 演示需要设 TCP_NODELAY 的位置
let stream = tokio::net::TcpStream::connect(addr).await?;
stream.set_nodelay(true)?;  // ← 必须
let ws = tokio_tungstenite::accept_async(stream).await?;
```

### 场景 4：ash-collab 的测试 TCP Server

`supervisor.rs` 中的测试 TCP Listener（935–973 行）使用 raw `TcpStream` 未设 `set_nodelay`。测试环境通常是 localhost，RTT ≈ 0，不会触发 Nagle 延迟；但如果测试改为跨网络或 Docker，可能出现偶发延迟。

## 解决方案

| 层面 | 方案 | 代码 |
|------|------|------|
| **TCP 层** | `TCP_NODELAY = true` 禁用 Nagle | `tcp_stream.set_nodelay(true)?` |
| **TCP 层** | 禁用 `tcp_slow_start_after_idle` | `sysctl -w net.ipv4.tcp_slow_start_after_idle=0` |
| **应用层** | 合并小帧 / 减少帧数 | 批量发送心跳、合并 JSON-RPC batch |
| **应用层** | 禁用 ping/pong 的单独帧 | WebSocket 库允许合并控制帧 |
| **OS 层** | 调小 delayed ACK 定时器 | `sysctl -w net.ipv4.tcp_delack_min=20`（最小 20ms） |
| **OS 层** | 调低 `tcp_retries1` | 不做丢包时的无用等待 |
| **框架层** | 确保 HTTP client 设 `tcp_nodelay` | `reqwest::Client::builder().tcp_nodelay(true)` |

### 推荐配置

**客户端 WebSocket：**

```rust
use tokio::net::TcpStream;

let mut stream = TcpStream::connect(addr).await?;
stream.set_nodelay(true)?;  // 禁用 Nagle，这是最关键的一步
let (ws_sender, ws_receiver) = tokio_tungstenite::client_async(url, stream).await?;
```

**服务端 WebSocket：**

```rust
let stream = tcp_listener.accept().await?;
stream.set_nodelay(true)?;
let (ws_sender, ws_receiver) = tokio_tungstenite::accept_async(stream).await?;
```

**HTTP Client（reqwest）：**

```rust
let client = reqwest::Client::builder()
    .tcp_nodelay(true)        // 默认就是 true，显式写以明确
    .http1_only()             // 避免 HTTP/2 HOL blocking
    .build()?;
```

**MCP 子进程通信（stdin/stdout）：**

```rust
// 子进程端确保每次写入后 flush
writeln!(stdout, "{}", json_rpc_line)?;
stdout.flush()?;
```

## Linux 内核参数调优（生产级 WebSocket 服务）

```bash
# /etc/sysctl.d/30-websocket.conf

# 禁用 Nagle（默认应用层设 NODELAY，kernel 可以兜底）
# ! 这会使所有 TCP 连接都用 NODELAY，按需配置
# net.ipv4.tcp_nodelay = 1

# 缩短 Delayed ACK 定时器（最小 20ms，默认 40-200ms）
net.ipv4.tcp_delack_min = 20

# 减少空闲后慢启动时间
net.ipv4.tcp_slow_start_after_idle = 0

# 增大 TCP 初始拥塞窗口
net.ipv4.tcp_initcwnd = 20

# 应用
# sysctl --system
```

## 诊断工具

```bash
# 抓包确认小包是否被延迟
sudo tcpdump -i any -nn port 8080 -w ws.pcap
# Wireshark 分析时注意对比：Seq 间隔 vs Time

# 查看 Nagle 是否启用（NODELAY 状态）
ss -tinfo | grep -E "nodelay|NODELAY"

# 查看 delayed ACK 超时
ss -tie | grep -oE "delack:[0-9]+"

# 查看当前 TCP 参数
sysctl net.ipv4.tcp_slow_start_after_idle
sysctl net.ipv4.tcp_delack_min
```

## 检查清单

- [ ] TcpStream 创建后调用了 `set_nodelay(true)`？
- [ ] HTTP client 显式设置了 `tcp_nodelay(true)`？
- [ ] 子进程的 stdout 写入后调了 `flush()`？
- [ ] 是否有偏长的 idle 间隔触发了慢启动？
- [ ] 多连接场景下，单个连接的小包是否被同进程的其他流干扰？
- [ ] 是否在 Docker 或虚拟化环境中？宿主机 TCP 参数可能不同？

## 参考

- RFC 896 — Nagle's Algorithm
- RFC 1122 §4.2.3.2 — Delayed ACK
- [Nagle's Algorithm is a cancer](https://www.stuartcheshire.org/papers/nagledelayedack/) — Stuart Cheshire 的经典分析
- [TCP_NODELAY and Nagle's algorithm (Julia Evans)](https://jvns.ca/blog/2016/06/30/why-do-we-need-tcp-nodelay/)
