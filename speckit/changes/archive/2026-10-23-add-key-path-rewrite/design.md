---
create-time: 2026-10-23 10:59:00
---

# Design

## Context

当前 `config.Entry` 持有 `Target` 和 `Models`；`keymap.Route` 的模型查找和轮询均返回 `config.Target`；`proxy.Prepare` 完成目标选择和模型名称重写，随后通过 context 向代理传递目标。`proxy.New` 的 Director 仅替换协议、主机和鉴权，不修改 `Path`、`RawPath` 和 `RawQuery`。普通上游和加密 peer 复用这一转发链。

新增规则必须随选中的配置项传递，而不能只按 token 保存，否则同 token 多上游会错误共享规则。还需维护 `net/url.URL` 的解码路径与编码路径一致性，避免将 `%2F` 当成新的路径分段，或把已有转义重复编码。

## Decisions

### 1. YAML 字段属于 target，启动时解析为有序规则

用户配置形式：

```yaml
keys:
  - token: sk-virt-dev
    target:
      base_url: https://api.openai.com
      api_key: sk-real-replace-me
      rewrite:
        - '/api:/v1'
        - '/openai:/v1'
```

为运行时增加 `RewriteRule` 类型（source 与 destination；删除标记 `-` 在解析时转为内部空 destination）。YAML 从每条 key 的 `target.rewrite` 读取字符串列表，解析后保存在该 entry 的运行时 `Target` 上，通过现有的模型选择、轮询、Prepare 和 context 返回链一起传递。原始字符串列表与解析后规则采用分离的解码表示，避免直接把字符串列表解码为结构体列表。`rewrite` 不放在与 `token`、`models` 同级的位置。

复用 Target 可避免改变所有路由方法的签名。增加切片后 Target 不再可用 `==` 比较，实现时检查并调整相关测试断言；构建路由时复制规则切片，并在发布后只读，保证并发安全。另一种方案是引入包含 Target 和规则的新选择结果类型，但本次会造成不必要的调用链改动。

校验从 target 节点下的 rewrite 节点读取 YAML 位置，错误指明 `keys[i].target.rewrite` 及规则索引和行号，要求列表项严格为字符串，避免 YAML 自动将数字等转换为字符串。缺省或空列表为无规则。每条规则必须有且仅有一个字面冒号；source 必须为非空、以 `/` 开头而非 `//` 开头的路径前缀；destination 必须为同样的路径前缀或单独的删除标记 `-`。空 destination 仍非法。路径前缀不含空白、query、fragment 或无效百分号转义，视作 escaped path 字面量，不解释正则。重复和重叠规则允许，顺序本身就是优先级。

### 2. 选路之后，在出站 Director 中执行一次重写

```text
鉴权 → 模型匹配或轮询 → 选定 Target（携带该 entry 的规则）
     → Director 重写路径 → 设置上游 origin / Host / Authorization
     → 普通或加密 transport
```

原始入站路径不用于重新选路，也不改变轮询计数或模型名称重写行为。Director 作用于 ReverseProxy 出站副本，避免修改原始请求路径。目标 host、peer 选择和权限不受规则影响。

### 3. 按原始 escaped path 首次命中，不执行连续替换

以 `r.URL.EscapedPath()` 为比较输入。对每条规则检查：路径等于 source；或以 source 开头，且 source 以 `/` 结束或紧随的字符为 `/`。命中后输出 destination 加未匹配后缀并立即返回。删除标记已转成空 destination，因此只保留后缀；若结果不以 `/` 开头则补一个 `/`，空结果也成为 `/`。例如 `/api:-` 将 `/api/v1/messages` 改为 `/v1/messages`，将 `/api` 和 `/api/` 改为 `/`；`/api/:-` 将 `/api/models` 改为 `/models`。删除后不再匹配后续规则。没有命中时不改任何 URL 路径字段。

不做最长匹配，不自动排序，不清理重复斜杠或点路径，不做大小写转换。具体规则应放在宽泛规则前。末尾斜杠按字面保留：`/api:/v1` 会将 `/api/` 改为 `/v1/`。如配置以斜杠结尾的 source 或 destination，按同一字面替换规则执行，不隐式添加或删除分隔符；仅删除标记模式会按上述规则补足绝对路径的开头斜杠。`/` 作为 source 可匹配所有绝对路径，配置者需在 destination 中明确所需的连接斜杠，例如 `/:/v1/`。

未另外约定的编码细节采用保守策略：按编码路径字面匹配，`/api%2Fmodels` 不匹配 `/api`，`/%61pi` 也不匹配 `/api`。这样不会因为解码意外扩大匹配范围。

### 4. 同步 Path 与 RawPath，保持 query 完整

命中后对完整的新 escaped path 做一次 `url.PathUnescape` 得到 `URL.Path`，并把新 escaped path 设置为有效的 `URL.RawPath`，以保留后缀转义和转义字母大小写。不要使用 URL resolve、path.Join 或完整 URL 字符串替换。启动校验保证规则转义有效，原始 EscapedPath 亦由 net/url 生成有效表示。

`RawQuery`、`ForceQuery` 和与路径无关的请求字段不变；尤其不能替换 query 中的 `/api`。未命中时连 Path / RawPath 的原始表示也保持不变。现有鉴权、Host、模型名称重写、响应透传和流式逻辑不变。

### 5. 保持配置兼容，补充文档和分层测试

不修改 bare-origin 校验，不引入依赖或运行时重载。README 与示例说明字段层级、首条命中、重叠顺序、字面斜杠和编码语义，并给出多入口映射及同 token 不同上游示例。

测试覆盖：配置形状和逐项错误位置；规则匹配表；真实 upstream 观察到的 RequestURI、Host 和 Authorization；同 token 模型选择和轮询的规则隔离；编码后缀与 query 字节保留；peer 转发；原有 SSE 和透明转发回归。

## Risks / Trade-offs

- **编码路径容易被二次编码**：同时维护 Path / RawPath，使用真实 HTTP 上游验证 `%2F`、`%20`、`%25` 和转义大小写，不只断言解码后的 Path。
- **规则顺序可能遮蔽具体规则**：保持用户声明顺序，在说明中强调具体规则优先；不引入自动最长匹配或规则链。
- **Target 内部携带切片改变可比较性**：调整断言并检查所有 Target 使用点，不使用 Target 作 map key；复制切片后只读。
- **字面规则不支持隐式解码或 URL 标准化**：有意选择可预测、较小的行为范围，并明确记录；正则、通配符、直接写空 destination 的规则、多次重写均不在本次范围；删除前缀必须显式使用 `-` 标记。
- **多跳网关可能每跳分别重写**：每个网关只应用本机选中的 entry 规则一次，不保证跨网关只重写一次；不增加跨跳状态或新 header。
