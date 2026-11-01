# Design

## Context

llm-gateway 是一个极简的虚拟 key LLM 代理（~20 个 Go 文件，唯一依赖 yaml.v3，Go 1.26）。请求管线：`server.New` 的单一 handler 做 Bearer 认证（`keymap.Map.Select`）→ `proxy.Prepare` 缓冲/改写 body → `httputil.ReverseProxy` 经 Director 改写 scheme/host/Authorization 后转发。当前没有任何加密、TLS 服务端、peer/cluster 概念；网关可以被朴素地链式部署（另一台网关的 listen 地址可作为 `base_url`），但链路完全明文。

约束（来自需求方拍板）：

- 只加密网关↔网关链路；网关↔客户端（明文 HTTP）与网关↔上游提供商（标准 HTTPS）保持现状。
- 无 TLS 可依托，流量在公网上裸奔 → 必须应用层加密 + 身份公钥 pinning 防主动中间人。
- 非对称协商（X25519），每连接一次握手，连接上所有请求复用会话。
- 配置形态对称，无客户端/服务端角色区分。
- SSE 流式逐块转发语义必须保持。

## Decisions

- **加密层级选在传输层（Noise 包裹 TCP 字节流），而非应用层信封。** 握手完成后，整条连接上的 HTTP 字节流（请求行、头、body、响应）全部经 Noise transport AEAD 加密。理由：HTTP 语义零改动，`ReverseProxy`、SSE（`FlushInterval: -1`）、头透传全部原样工作；分帧与防重放由 Noise transport 的消息序号（滑动窗口）原生保证，无需自研帧格式。代价：每方向每连接一次握手（1-RTT）。
- **协议选 Noise IK 模式。** IK 适用于「发起方已知响应方静态公钥」的场景，正好匹配配置中钉死的 `peers[].identity_pub`：发起方第一个消息即携带被加密的身份 `s`（只有持有对端私钥者可解），响应方解密后校验发起方身份是否在本机 peers allowlist 中；双方静态密钥参与 `es/ss` 派生，提供双向认证 + 前向安全（临时密钥）。
- **依赖 `github.com/flynn/noise` v1.1.0 的 Noise 实现**（Go 生态事实标准、纯 Go、无 cgo），不自拼 ECDH+AEAD 协议。实现阶段核实：`golang.org/x/crypto/noise` 并不存在（x/crypto 全版本均无 noise 包，原 design 假设有误），flynn/noise 即正牌 Noise 协议库。
- **e2ee 监听使用独立端口**（`e2ee.listen`），与客户端入口 `listen` 分离。理由：同一端口上区分「明文客户端流量」与「Noise 握手流量」需要协议嗅探，复杂且脆弱；独立端口让防火墙规则也更直观。
- **接收侧管线完全复用**：e2ee 监听器 accept TCP → Noise 握手/包裹 → 以包裹后的 conn 组成 `net.Listener` 交给同一个 `http.Server` handler（认证、模型路由、转发逻辑一行不改）。发起方身份认证失败的连接在握手层直接断开，到不了 handler。
- **发起侧在 Transport.DialContext 做包裹**：`proxy.New` 接收 peer 拨号表（`map[host:port]NoiseDialer`）；DialContext 命中 peer 地址时先拨 TCP 再 Noise 包裹后返回。连接池按地址复用，握手自然成为「每连接一次」。Director 只需照常把 `target.base_url`（= peer 的 e2ee 地址）写进 scheme/host。
- **配置模型**：`e2ee{listen, identity_key, peers[]{name, base_url, identity_pub}}`；`keys[].target` 新增 `peer` 字段，解析时把 peer 名称展开成其 `base_url` 并标记该 target 走加密（target 上携带 peer 名，启动时校验 peer 地址不与任何明文 `base_url` 的 host:port 冲突，保证 DialContext 仅凭地址即可无歧义决定是否包裹）。
- **身份密钥**：X25519 静态密钥对，32 字节、base64 编码存配置（沿用项目「配置即真相」的极简风格）。`--keygen` 用 `crypto/ecdh`（stdlib）生成。Noise 协议名固定（如 `Noise_IK_25519_ChaChaPoly_BLAKE2s`，x/crypto 默认密码套件），双方一致即可。
- **日志**：e2ee 连接建立/拒绝以独立 event 记录（peer 名、远端地址、结果），绝不输出密钥材料；沿用现有 slog JSON 风格。

## Risks / Trade-offs

- ~~x/crypto noise 模块路径/可用性不确定~~（已解决：`golang.org/x/crypto` 不含 noise 包，改用 `github.com/flynn/noise` v1.1.0，无回退需要）。
- **每连接 1-RTT 额外延迟**：仅在新 TCP 连接时发生；HTTP keep-alive 下多请求摊薄。可接受，不做会话恢复（记录为未来优化）。
- **单向泄露场景**：`identity_key` 泄露允许攻击者冒充该网关；peers 是静态列表，无撤销机制（换密钥需双边更新配置）。v1 接受，文档中说明轮换流程。
- **e2ee 端口直接暴露公网**：接收侧对任何 TCP 连接都做握手，未知身份在握手层被拒；无速率限制，存在握手 DoS 放大面（Noise 握手有公开的 DoS 防护模式可选）。v1 不做限速，README 记录建议配合防火墙/安全组只放行已知 peer IP。
- **`Content-Encoding` 与 e2ee 的交互**：现有 `Prepare` 对非 identity 编码跳过 body 检查；e2ee 在传输层，不影响此语义（转发的是已加密管道内的完整 HTTP），无需变更。
