---
name: llm-proxy-data-troubleshooting
description: "Inspect java-llm-proxy request data: proxy.db schema, per-session request/response bodies, SSE parsing, reasoning replay decisions and provider routing diagnosis."
whenToUse: "Use when a client (dcl, Codex, OpenCode) gets an unexpected reply, missing thinking, 429/503 errors, or when you need the exact bytes the proxy recorded for a session."
user-invocable: true
disable-model-invocation: false
---

# 排查 java-llm-proxy 的数据

代理把每一次上游请求都写进 `cpa-core/data/proxy.db`。排查客户端问题时，先定位 session，再看四份 body 的差异，不要从日志猜。

## 数据位置

| 路径 | 内容 |
| --- | --- |
| `cpa-core/data/proxy.db` | 请求记录（SQLite，可能 2 GB+） |
| `cpa-core/logs/core-start-output.log` | 后端启动与错误堆栈 |
| `cpa-core/logs/log.YYYYMMDDHH.0.log` | 运行日志 |
| `cpa-core/config.yaml` | providers、models、routing、access |
| `oauth/*.json` | OAuth 账号凭据（含 token，切勿输出） |

Windows 便携版安装目录示例：`D:\dev_program\LLMPoroxyUI-v1.0.0-Windows-amd64`；开发仓库示例：`D:\code\project\project-llm-proxy\java-llm-proxy`。路径以实际安装位置为准。

## requests 表

```sql
id TEXT PRIMARY KEY, timestamp INTEGER, model TEXT, provider TEXT, api_key TEXT,
source TEXT, status INTEGER, duration INTEGER, input_tokens INTEGER, output_tokens INTEGER,
cached_tokens INTEGER, reasoning_tokens INTEGER, cost REAL, summary TEXT, body TEXT,
originator TEXT, session_id TEXT, thread_id TEXT
```

`body` 是 JSON，关键字段：

| 字段 | 含义 |
| --- | --- |
| `requestBody` | 客户端发来的原文（协议=客户端协议） |
| `upstreamRequestBody` | 转换后真正发给上游的 body |
| `responseBody` | 返回给客户端的 SSE / JSON |
| `upstreamResponseBody` | 上游返回的 SSE / JSON |
| `requestHeaders` / `upstreamRequestHeaders` | 头（authorization 已 redacted） |
| `protocol` / `path` / `status` / `error` | 协议、端点、状态、错误 |

## 定位顺序

1. 找 session：`select session_id, originator, source, count(*) from requests group by 1,2,3 order by 4 desc limit 20;`
   - `originator`：`dcl`、`codex-tui`、`claude-code-cli` 等。
   - `source`：user-agent。
2. 列该会话的回合：
   ```sql
   select id, timestamp, status, model, provider, input_tokens, output_tokens, reasoning_tokens, summary
   from requests where session_id=? order by timestamp;
   ```
3. 读某一条：`select body from requests where id=?;`（主键，最快）。
4. 对比 `requestBody` → `upstreamRequestBody` → `upstreamResponseBody` → `responseBody`。差异就是代理做的转换。

大字段不要整段打印。先 `select length(body) ...`，再用 `scripts/peek_session.py` 摘要。

## 常见症状

| 症状 | 查什么 | 结论 |
| --- | --- | --- |
| 客户端看不到 thinking | `upstreamResponseBody` 里 `"reasoning_content"` 数量 vs `responseBody` 里 `response.reasoning_summary_text.delta` 数量 | 前者有、后者 0 = 代理没转换；两者都有 = 客户端没渲染 |
| `response.completed.output` 出现 `"text":""` 的 message | 工具调用轮次 | 旧版代理会凭空造空 assistant message；新版只在有文本时创建 |
| 客户端回放历史后报 `invalid_encrypted_content` | `requestBody` 里 reasoning item 的 `encrypted_content` vs `upstreamRequestBody` | 代理判定签发者不同会剥掉密文；`x-dcl-session-id` 缺失会全部按“外来”处理 |
| 503 `No enabled provider for this model and protocol` | `config.yaml` 的 `models` / `excluded-models` / `routing` | 模型不在 provider.models、或被 excluded-models 排除、或 provider disabled |
| 429 `usage_limit_reached` | `upstreamResponseBody` + `provider` | 订阅账号额度耗尽，换 provider 或等 reset |
| 上游 502 `Stream interrupted` | `upstreamResponseBody` 末尾 | 上游/网络中断，非协议转换问题 |
| 请求路由到了错误后端 | `provider` 字段 | 看候选顺序与 `routing.strategy`（round-robin / session-affinity） |

## SSE 解析

`responseBody` / `upstreamResponseBody` 是原始 SSE 文本：事件以空行分隔，`event:` 行给类型，`data:` 行给 JSON，末尾 `data: [DONE]`。

```python
def frames(text):
    for block in text.split("\n\n"):
        ev, data = None, []
        for line in block.split("\n"):
            if line.startswith("event:"): ev = line[6:].strip()
            elif line.startswith("data:"): data.append(line[5:].strip())
        if data and data != ["[DONE]"]:
            yield ev, json.loads("\n".join(data))
```

## 安全与性能

- 只读打开：`sqlite3.connect(f"file:{db}?mode=ro", uri=True)`。
- 不要 `select *` 全表；2 GB 库会拖慢甚至占满内存。
- 不要输出 `authorization`、`api_key`、`oauth/*.json` 的内容；库里的头已 redacted，但 `requestBody` 可能含用户数据。
- 每行 body 最大可达数 MB；先 `length(body)`。

## 脚本

`scripts/peek_session.py <proxy.db> <session_id>`：输出回合时间线、每轮状态/模型/后端/token、客户端与上游的 reasoning 数量、空 message 检测。
