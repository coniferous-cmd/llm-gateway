# Proposal

## Why

llm-gateway 可以被链式部署：网关 B 把请求转发给网关 C，再由 C 转发给上游 LLM 提供商。今天这种网关↔网关链路完全依赖公网本身 —— 两台网关之间的 URL、请求体、Authorization（此时已是真实 API key）全部明文裸奔。部署方没有为网关间链路配置 TLS 的手段（网关自身只监听明文 HTTP，TLS 终止留给外部），因此需要应用层的端到端加密来保护跨公网的网关间流量。

## What Changes

- 新增 `internal/e2ee` 包：网关之间使用 Noise IK 协议（X25519 非对称身份协商 + 前向安全的每连接会话密钥）加密整条 TCP 字节流，HTTP 语义（含 SSE 流式）保持不变。
- 每台网关持有对称的身份密钥对；身份公钥在配置的 `peers` 列表中互相钉死（pinning），在无 TLS 的公网上防御主动中间人。
- 新增 `e2ee` 配置段：`listen`（网关间加密入口，独立端口）、`identity_key`（身份私钥）、`peers`（对端列表：名称、对端 e2ee 地址、对端身份公钥）。
- `keys[].target` 新增 `peer` 字段：把该条目指向某个已声明的 peer，请求经 Noise 加密管道转发给对端网关；`base_url` 与 `peer` 二选一。
- 新增 `--keygen` CLI 选项：生成并输出身份密钥对，便于运维在两台网关之间带外交换公钥。
- 网关↔客户端、网关↔上游提供商链路保持现状（明文 HTTP / 标准 HTTPS），不做任何加密变更。
- 网关间无客户端/服务端角色区分：配置形态对称，谁发起 TCP 连接谁发起 Noise 握手，双方都可主动加密调用对端。

## Impact

- Affected specs: `specs/gateway-e2ee/spec.md`（新增能力）
- Affected code:
  - `internal/e2ee/`（新包：Noise IK 握手、传输包裹、身份密钥生成/解析）
  - `internal/config/config.go`（`e2ee` 配置段、`target.peer`、校验规则）
  - `internal/proxy/proxy.go`（Transport DialContext 对 peer 地址做 Noise 包裹）
  - `internal/server/server.go`（e2ee 监听器复用同一请求处理管线）
  - `cmd/llm-gateway/main.go`（`--keygen`、启动 e2ee 监听）
  - `go.mod`（新增 `github.com/flynn/noise` 依赖，纯 Go 无 cgo）
  - `config.example.yaml`、`README.md`（文档与示例配置）
