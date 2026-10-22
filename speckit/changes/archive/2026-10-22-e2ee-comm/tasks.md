# Tasks: e2ee-comm

## 1. 依赖与 Noise 库落地

- [x] 1.1 确认 Noise 实现的可用模块路径：`golang.org/x/crypto` 不含 noise 包（全版本核实），改用 `github.com/flynn/noise` v1.1.0（Go 生态事实标准，纯 Go 无 cgo）并写入 go.mod；`go build ./...` 通过。
- [x] 1.2 新建 `internal/e2ee/identity.go`：X25519 身份密钥对生成、base64 编解码、校验（长度/格式）。验证：`identity_test.go` 9 用例全通过。

## 2. internal/e2ee 核心

- [x] 2.1 新建 `internal/e2ee/session.go`：Noise IK 握手两端，Session 实现 net.Conn（每 Write 一帧 AEAD、Read 解帧缓冲）；发起方钉死公钥、响应方 allowlist 校验。验证：成功握手、错误 pin 拒绝、未知身份拒绝、大 payload 分帧、篡改帧拒绝 —— 全通过。
- [x] 2.2 新建 `internal/e2ee/conn.go`：`Dialer.DialContext`（匹配 http.Transport 签名，非 peer 地址拒拨）、`Listener.Accept`（握手失败记日志关连接继续等待）。验证：TCP echo 往返、未配置地址拒绝、未授权身份拒绝（不外泄会话）、SSE 屏障测试证明逐块到达 —— 全通过。
- [x] 2.3 新建 `internal/e2ee/keygen.go`：`GenerateKeyPair` 输出两行可粘贴配置。验证：输出两行可直接粘贴进配置，私钥不重复打印。

## 3. 配置集成

- [x] 3.1 `internal/config/config.go`：`E2EE{Listen, IdentityKey, Peers[]{Name, BaseURL, IdentityPub}}` 与 `Target.Peer`；校验互斥/引用/origin/base64/地址冲突/行号。验证：`TestParseE2EE` 15 用例全通过（含行号断言、默认 listen、冲突检测）。
- [x] 3.2 `internal/keymap`：`Target.Peer` 透传确认。验证：`TestPeerTargetPassthrough` —— Resolve/RoundRobin/Context handoff 均保留 Peer 标记 —— 通过。

## 4. 转发与服务集成

- [x] 4.1 `internal/proxy/proxy.go`：`New(peers)` 拨号表按目标地址分流（peer → Noise，其他 → 明文）；`New()` 调用点全部更新为 `New(nil)`。验证：`TestPeerForwardingOverE2EE` 经 Noise 链路转发 auth/path/body 全保留，明文用例不改通过 —— 35/35。
- [x] 4.2 `internal/server/server.go`：`NewWithPeers` 共享同一 handler 管线；`Serve(ctx, srv, ln)` 支持在 e2ee 包装 listener 上服务。验证：现有 7 用例不改通过。
- [x] 4.3 `cmd/llm-gateway/main.go`：`--keygen`；`buildE2EE` 解析身份/拨号表/allowlist；双 server 共享 ctx 优雅退出。验证：`TestCLIChainedGatewaysOverE2EE` 真实双进程 B→C 加密链式转发（含 --keygen 工作流）、`TestCLIKeygen` —— 全通过。

## 5. 安全与流式回归

- [x] 5.1 安全用例：错误 pin（MITM）双向拒绝且无应用数据、未授权身份不外泄会话、`TestLogsNeverContainPrivateKeyMaterial` 断言私钥（base64/hex）不出现在拒绝日志 —— 全通过。
- [x] 5.2 SSE 回归：`TestSSEChunksArriveIncrementally`（e2ee 层屏障测试）+ `TestSSEStreamingOverE2EE`（proxy 全链路，首事件先于上游完成到达）—— 全通过。

## 6. 文档与示例

- [x] 6.1 `config.example.yaml` 增加 e2ee/peer 注释示例；`README.md` 增加 "Encrypted gateway-to-gateway links (e2ee)" 章节：拓扑图、--keygen 流程、pinning 信任锚、防火墙建议、轮换流程。验证：README 演练流程由 `TestCLIChainedGatewaysOverE2EE` 用真实二进制自动化验证。
- [x] 6.2 `go vet ./...` 通过；`go test ./...` 126/126 全过（7 包）。`-race` 需要 C 工具链，本机（Windows/无 gcc）不可用 —— 与 README 既有约束一致，CI 在 Linux 上跑 race。
