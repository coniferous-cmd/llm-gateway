---
create-time: 2026-10-23 11:01:57
---

## 1. Implementation

- [x] 1.1 在 `internal/config` 增加每条 key 的 `target.rewrite` 列表解析和运行时规则类型；使用 YAML node 校验列表及字符串项、唯一冒号、绝对路径前缀和转义合法性，错误包含 `keys[i].target.rewrite`、规则索引及行号。添加合法多规则、缺省、空列表和各类无效输入的表驱动测试，并保持 bare-origin 校验不变。
- [x] 1.2 将解析后的规则随运行时 Target 传递，更新 `internal/keymap` 的必要复制和断言；验证同 token 不同 entry 的模型匹配和轮询均保留自身规则，且不改变轮询推进语义。检查新增切片导致的 Target 比较编译问题。
- [x] 1.3 在 `internal/proxy` 增加 escaped path 的有序前缀重写逻辑，并在 Director 中选路后执行一次；同步 Path / RawPath，保持 RawQuery / ForceQuery 不变。添加精确匹配、后代路径、斜杠边界、未命中、首条优先、无连续替换、大小写、根路径及字面尾斜杠测试。
- [x] 1.4 增加真实 HTTP 上游集成测试，断言 RequestURI 的路径重写和 query 字节完整性；覆盖 `%2F`、`%20`、`%25`、转义大小写、编码分隔符不匹配、无规则时原样转发及原始入站 URL 不被修改。
- [x] 1.5 验证模型选路与路径重写组合、同 token 轮询规则隔离、加密 peer 的单跳重写，并回归 Host / Authorization、其他请求头、body、SSE 与响应错误透传行为。

## 2. Documentation and verification

- [x] 2.1 更新 `README.md` 和 `config.example.yaml`，演示位于 target 内、与 base_url / api_key 同级的 rewrite 列表、多入口映射和同 token 不同上游；说明首次命中即停止、具体规则靠前、无匹配不变、查询参数不改及字面斜杠/编码限制，修订“路径始终原样转发”的说明。
- [x] 2.2 运行 `gofmt`（仅格式化本次修改的 Go 文件）、`go test ./...` 和 `go vet ./...`；环境支持时运行 `go test -race ./...`，否则记录平台限制和替代验证。确认旧配置及示例配置仍通过解析。
- [x] 2.3 对照增量规格逐项检查测试覆盖，运行 `speckit validate add-key-path-rewrite --strict`，记录最终验证结果及任何剩余限制。

## 3. Prefix deletion marker

- [x] 3.1 支持 destination 为 `-` 的删除前缀标记；空 destination 仍非法。删除后保持绝对路径，空结果为 `/`，仍按完整段匹配并只执行首条命中规则。
- [x] 3.2 添加配置、路径和真实 HTTP/加密 peer 测试，覆盖精确路径、尾斜杠、边界、编码、query 及无连续重写；更新 README 和示例。
- [x] 3.3 更新提案、增量规格、设计并重新验证，同步主规格并核验后继续归档。

## Verification results

- 已对本次修改的 Go 文件运行 `gofmt`；`git diff --check` 通过。
- `go test -count=1 ./...`：7 个包、256 项测试通过（含 `-` 删除标记增量）；旧配置解析和更新后的 `config.example.yaml` 均有测试验证。
- `go vet ./...`：通过。
- `go test -count=5 ./internal/config ./internal/keymap ./internal/proxy ./internal/server`：1070 项测试通过（包含现有并发轮询测试）。
- `speckit validate add-key-path-rewrite --strict`：通过。
- `speckit validate --specs`：两个主规格均通过；逐项比较确认本变更全部 4 个 requirement 的描述与场景均已合并，既有场景标题与其余 requirement 保留。
- **竞态检测未完成**：`go test -race ./...` 报告需要启用 CGO。当前环境为 Windows/amd64、`CGO_ENABLED=0`，PATH 未发现 GCC/Clang；重复运行常规测试仅作为补充，不等同于竞态检测。应在具有兼容 C 工具链的环境或 Linux CI 中执行 `CGO_ENABLED=1 go test -race ./...`。

### Spec coverage

- 配置列表、保留顺序、缺省与空列表、错误定位、peer 配置、bare-origin 兼容：`internal/config/rewrite_test.go` 和现有 `config_test.go`。
- 删除标记 `-` 的配置校验（与空目标和 `/-` 区分）、精确路径归根、尾斜杠删除、编码与 query 保留、模型选路/轮询/加密 peer 的组合，均有新增或扩展测试。
- 精确/后代匹配、完整段边界、首条优先、多入口、无连续替换及编码边界：`internal/proxy/rewrite_test.go`。
- 同 token 的模型选路、轮询及独立规则传递：`internal/keymap/rewrite_test.go`、`TestPrepareAndRewriteSelection`。
- origin 拼接、未命中路径不变、query 和编码字节保留、Host/Authorization/body/header/错误响应与原始入站 URL 保持：`TestRewriteUpstreamRequestURI` 及现有透明转发测试。
- SSE 逐块透传、加密 peer 的单跳重写及响应保留：`TestSSEFlush`、`TestSSEStreamingOverE2EE`、`TestPeerPathRewrite` 及现有 peer 测试。
