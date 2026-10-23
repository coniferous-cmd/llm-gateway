---
create-time: 2026-10-23 10:58:29
---

# Proposal

## Why

客户端使用的 API 路径前缀可能与上游要求不同。目前网关原样转发路径，而 `base_url` 仅允许 origin，无法把 `/api/chat/completions` 转为上游的 `/v1/chat/completions`。需要让每条 key 配置独立声明路径重写规则，而不要求客户端或上游修改接口。

## What Changes

- 在每条 `keys` 配置的 `target` 内增加可选的 `rewrite` 字符串列表（`target.rewrite`，与 `base_url`、`api_key` 同级），例如 `['/api:/v1', '/openai:/v1']`。
- 规则只匹配完整路径段前缀，按声明顺序首次命中后停止；不做正则、任意位置替换或连续重写。
- destination 为 `-` 时删除匹配前缀，例如 `/api:-`；删除结果保持绝对路径，空结果使用 `/`。空 destination（`/api:`）仍非法。
- 根据模型匹配或轮询选定配置项后，使用该项的规则重写请求路径，再使用选定目标的 origin 转发。查询参数保持原样。
- 未配置、空列表或无匹配时维持原有路径；加载配置时拒绝无效规则并报告位置。
- 更新示例、说明和测试，覆盖规则顺序、路径边界、编码路径及同 token 多上游隔离。

## Impact

- Affected specs: `virtual-key-proxy`（配置加载、透明转发，以及新增的每配置项路径重写要求）。
- Affected code: `internal/config` 的解析校验及配置类型，`internal/keymap` 的选中目标数据传递，`internal/proxy` 的出站 URL 处理及相关测试；`README.md`、`config.example.yaml`。
- 无需新依赖，不修改 `base_url` 的 bare-origin 限制；未使用新字段的配置保持兼容。
- 对 `target.base_url` 和 `target.peer` 采用相同的单跳重写语义，不改变加密传输和现有模型路由、鉴权、流式响应行为。
