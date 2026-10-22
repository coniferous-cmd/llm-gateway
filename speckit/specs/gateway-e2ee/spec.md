# gateway-e2ee Specification

## Purpose

TBD — the introducing delta had no Purpose section; write this before relying on it.

## Requirements

### Requirement: 网关间加密链路

系统 SHALL 在网关↔网关链路上对完整的 TCP 字节流进行端到端加密（Noise IK，X25519），使 URL、请求路径、请求头（含 Authorization）与请求体在公网上均不以明文出现。网关↔客户端链路与网关↔上游提供商链路 SHALL 保持现状（明文 HTTP / 标准 HTTPS）。

#### Scenario: 两台网关间转发全部字段加密

- **WHEN** 网关 B 收到客户端请求且路由目标配置为 `peer: gateway-c`，经 Noise 加密管道转发给网关 C
- **THEN** B→C 链路上的 HTTP 请求行、路径、查询串、全部请求头与请求体均为密文，C 解密后收到的请求与 B 侧发出的语义完全一致

#### Scenario: 客户端与上游链路不受影响

- **WHEN** 客户端以明文 HTTP 调用网关 B，网关 C 以标准 HTTPS 调用上游提供商
- **THEN** 行为与未启用 e2ee 时完全一致（虚拟 key 认证、模型改写、SSE 流式均不受影响）

### Requirement: 身份公钥钉死与双向认证

每台网关 SHALL 持有一对 X25519 身份密钥；发起方 SHALL 在握手时验证对端身份公钥与配置中钉死的 `peers[].identity_pub` 一致；接收方 SHALL 验证发起方身份公钥属于已配置的 peer 列表。验证失败时 SHALL 立即断开连接且不交换任何应用数据。

#### Scenario: 对端公钥不匹配时拒绝连接

- **WHEN** 发起方配置中钉死的 `identity_pub` 与实际对端身份公钥不一致（如遭遇主动中间人）
- **THEN** 握手失败，连接被断开，不发送任何 HTTP 应用数据，记录一条错误日志

#### Scenario: 未配置的发起方被拒绝

- **WHEN** 一台身份公钥不在本机 `peers` 列表中的网关尝试建立 e2ee 连接
- **THEN** 接收方在握手中认证失败并断开连接

### Requirement: 每连接一次握手的会话密钥

加密会话 SHALL 以 TCP 连接为粒度：每条连接建立时完成一次 Noise IK 握手，派生具备前向安全性的传输密钥，该连接上的所有后续请求（含 keep-alive 复用）复用同一会话；新连接 SHALL 重新握手。会话 SHALL 按方向独立加密并防重放。

#### Scenario: 同一连接上的多个请求复用会话

- **WHEN** 客户端网关在同一 TCP 连接上连续发起两个 LLM 请求
- **THEN** 仅第一个请求前发生一次握手，两个请求均在该会话密钥下加密传输

#### Scenario: 新连接重新握手

- **WHEN** 前一条 TCP 连接关闭后发起新的转发
- **THEN** 新连接重新执行完整握手，不复用已销毁的会话状态

### Requirement: 对称配置与 peer 路由

e2ee 配置 SHALL 在所有网关上形态一致（无客户端/服务端角色区分）：`e2ee.listen` 声明本机加密入口，`e2ee.identity_key` 为本机身份私钥，`e2ee.peers` 列出可信对端（名称、对端 e2ee 地址、对端身份公钥）。`keys[].target.peer` SHALL 引用已声明的 peer 名称并将请求经加密链路转发；`target.base_url` 与 `target.peer` SHALL 互斥且必居其一。配置 SHALL 在启动时完成严格校验并给出带行号的错误。

#### Scenario: target.peer 引用未声明的 peer

- **WHEN** 配置中 `target.peer` 指向 `e2ee.peers` 中不存在的名称
- **THEN** 启动失败并报告带行号的校验错误

#### Scenario: base_url 与 peer 同时出现

- **WHEN** 同一个 target 同时配置了 `base_url` 与 `peer`
- **THEN** 启动失败并报告带行号的校验错误

#### Scenario: 双向对称调用

- **WHEN** 网关 B 与网关 C 互相在对方的 `peers` 列表中
- **THEN** 任一方均可主动经加密链路调用另一方，无需任何角色配置差异

### Requirement: 身份密钥生成

系统 SHALL 提供 `--keygen` 选项，生成一对 X25519 身份密钥并以可直接粘贴进配置的格式输出（私钥供本机 `e2ee.identity_key`，公钥供对端 `peers[].identity_pub`）。私钥 SHALL 不出现在任何日志中。

#### Scenario: 生成身份密钥对

- **WHEN** 运行 `llm-gateway --keygen`
- **THEN** 输出一对 base64 编码的 X25519 私钥与公钥，退出码为 0，不启动服务

### Requirement: SSE 流式经加密链路保持逐块转发

加密链路 SHALL 保持 SSE 流式的逐块刷新语义：上游提供商发送一个数据块后，接收侧网关 SHALL 在解密该块后立即转发给下游，不得等待完整响应缓冲。

#### Scenario: 流式响应经 e2ee 链路逐块到达

- **WHEN** 客户端经 B→C 加密链路请求一个流式（SSE）LLM 响应
- **THEN** 客户端逐块收到增量数据，端到端延迟不因加密引入整段缓冲
