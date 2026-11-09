---
create-time: 2026-11-09 09:49:12
---

## 1. Implementation

- [x] 1.1 在 `Cargo.toml` 添加 `async-compression`（`default-features = false`，features `tokio`, `gzip`, `zlib`, `brotli`, `zstd`），确认 `cargo metadata` 解析成功；在 Windows/MSVC 与一个 `cargo cross` 目标上各构建一次，记录 `zstd-sys` 的 C 工具链需求。若某个目标无法提供 C 工具链，先把 `zstd` 从支持集移除并同步设计记录，不静默降级。
- [x] 1.2 在 `src/proxy/forward.rs` 实现 `Accept-Encoding` 改写：聚合所有字段行，按逗号拆分并解析 `coding[;q=]`；保留 `{gzip, deflate, br, zstd}` 中 q>0 的编码，显式项覆盖 `*`，交集为空时删头，调用方未发送时不添加。固定输出顺序为 `gzip, deflate, br, zstd`。单元测试覆盖多字段行、通配符、q 值、`q=0`、仅不支持编码和缺省头，并断言其他请求头不受影响。
- [x] 1.3 新建 `src/proxy/decode.rs`：实现 `Coding`、`classify`（大小写不敏感、单编码、`identity`、空值、多层与未知编码返回错误）、`BodyBufReader`（基于 `ReqBody` 的 `AsyncBufRead`，区分 Pending 与 EOF，跳过 trailer 帧）以及 `ReadBody`（把解码后的 `AsyncRead` 转回 `Body`，错误包装为 `DecodeError`）。`deflate` 映射到 zlib 解码器。
- [x] 1.4 在 `src/server/mod.rs` 接入解码：响应 `into_parts()` 后分类；受支持的单编码移除 `Content-Encoding`、`Content-Length`、`Content-MD5` 并把 `ReadBody` 放在 `ObservedBody` 内侧；`expected_response_bytes` 设为未知；未知或多层编码在写任何 body 字节前返回本地 `502`，category `upstream_encoding`；`HEAD`/`204`/`205`/`304` 原样透传；`is_end_stream()` 已为真的空 body 直接剥头按空处理；解码器在零输入、零输出时出错视为正常空流。
- [x] 1.5 在 `ObservedBody` 的错误路径区分 `DecodeError`（category `upstream_decode`）与其他 body 错误（保持 `upstream_read`）；确认诊断只含固定 category，且解码失败时 `response_complete=false`、`observed_bytes` 为已交付的明文字节数。
- [x] 1.6 为 `decode.rs` 增加往返测试：gzip/zlib/br/zstd 各编码在头部中切分、块中切分、尾部切分等边界上分帧输入，断言输出字节与原文一致；另测截断输入、损坏输入、空输入、`identity` 透传和多层编码拒绝。
- [x] 1.7 扩展 `tests/http_e2e.rs` 的上游测试替身，使其仅在请求声明支持时压缩响应；新增端到端用例：gzip 客户端收到无 `Content-Encoding`/`Content-Length` 的明文、仅 `br` 客户端、仅不支持编码的客户端、未发送 `Accept-Encoding`、通配符与 q 值、gzip SSE 首事件在剩余事件之前到达、上游返回未协商编码时 502 且无 encoded body 字节、`HEAD`/`304` 头部不变、空 body 带编码、截断 gzip 导致不完整记录。
- [x] 1.8 增加会话记录端到端断言：启用 `conversation_log` 后，压缩响应的记录内容为明文且 `encoding` 为 `utf-8`（或二进制时为 base64）、`observed_bytes` 为解压后计数；压缩请求体仍按原始字节记录；解码失败记录 `response_complete=false` 且 category 为 `upstream_decode`。

## 2. Documentation

- [x] 2.1 更新 `README.md` 的转发边界说明：删除 “no automatic decompression”，写明始终向客户端交付明文、上游协商集合、502 与解码失败语义、无 body 响应例外、请求体不解压。
- [x] 2.2 更新 `docs/parity-checklist.md`：修订 Forwarding 与 Body inspection 条目，并在有意分歧清单中登记“响应透明解压 / `Accept-Encoding` 网关控制”，说明原因和与 Go 参考实现的差异。
- [x] 2.3 检查 `README.md` 会话文件章节是否需要补充“响应按解码后明文记录、请求体保持原始字节”，并确认 `config.example.yaml` 无需新增字段。

## 3. Verification

- [x] 3.1 运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test --all-targets`，全部通过。
- [ ] 3.2 在 Windows 与 `cargo cross` 目标上运行构建（含 `zstd-sys`），记录结果；在可用环境中补跑网络相关端到端测试。（Windows/MSVC 构建与本地端到端测试已完成；`cargo cross` 目标因本机无可用 cross/Docker 守护进程，留待 CI 验证。）
- [x] 3.3 人工核对 `speckit/specs/virtual-key-proxy` 与 `speckit/specs/conversation-file-logging` 的 delta 覆盖：每个新增场景都有对应测试或明确的不可自动化验证说明。
- [x] 3.4 运行 `speckit validate transparent-decompression --strict`，修复所有问题后记录最终验证结果与剩余限制（尤其是 `deflate` raw 兼容性和 206 范围响应）。
