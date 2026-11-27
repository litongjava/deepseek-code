---
name: dc-docs
description: "DeepSeek Code（dc，包括 CLI dcl 与 Desktop dcd）的使用与排障指南：调用 dcl 执行任务、运行中发消息引导、审查执行步骤、DEEPSEEK_CODE_DATA_DIR 布局、大模型请求分析、测试分层、REPL 面板及 MCP 服务器配置。"
whenToUse: "当需要使用 dc（deepseek-code，含 CLI dcl 与 Desktop dcd）执行任务、在运行中向 dcl 发送消息或审查其执行过程、定位 DEEPSEEK_CODE_DATA_DIR 数据目录、分析 dc 发往大模型的 HTTP 请求、驱动 REPL 面板、验证 CLI/core 改动，或配置排障 MCP 服务器时使用。"
user-invocable: true
disable-model-invocation: false
---

# DeepSeek Code（dc）使用与诊断指南

`dc` 指 DeepSeek Code，包含命令行版本 **CLI（`dcl`）** 与桌面端 **Desktop（`dcd`）**。两者共用同一个 Rust 引擎、数据目录（`DEEPSEEK_CODE_DATA_DIR`）、权限规则、MCP 配置和工具实现。

本 skill 自带资源（路径相对本 skill 目录）：

- `scripts/inspect_dcl.py`：零外部依赖的审计脚本，支持 `session`（分析 `session.db` 耗时/缓存/工具/Shell 反模式）、`stream`（实时或事后解析 `stream-json` 运行流）与 `requests`（检查 `request-records` 工具列表及相邻请求前缀 diff）。
- `scripts/probe_mcp.py`：stdio MCP 握手、`tools/list` 与可选 `tools/call` 探针。
- `scripts/drive_dcl.py`：通过 ConPTY 驱动 `dcl` 并用 `pyte` 渲染屏幕。
- `references/pty-harness.md`：PTY/ConPTY 细节与临时数据目录初始化。

---

## 1. 调用 dcl 执行任务

### 1.1 非交互执行（推荐用于脚本、CI、代理编排）

`-p` / `--print` 会运行完整工具循环后退出。常用启动模板：

```bash
dcl -C /path/to/repo \
  --provider opencode-go \
  --model deepseek-v4.1-flash \
  --effort max \
  --permission-mode bypass \
  --print --output-format stream-json --verbose \
  --record-requests \
  < task.md > run.jsonl 2> run.err
```

常用参数与实战要点：

| 参数 | 说明 |
| --- | --- |
| `-C, --path <dir>` | 指定目标项目/worktree 目录（默认当前目录），避免模型开局频繁 `cd`。 |
| `--provider` / `-m, --model` | 指定提供商与模型 ID（也支持 `provider::model`）。 |
| `--effort <level>` | 思考强度：`off`、`low`、`medium`、`high`、`xhigh`、`max`。 |
| `-p, --print` | 非交互执行单轮完整任务后退出。 |
| `--output-format` | `text`、`json` 或 `stream-json`；编排与审计优先用 `stream-json --verbose`。 |
| `--permission-mode` | 非交互模式无人点审批，`ask` 下写操作会被拒；按场景用 `--allowed-tools`、`--permission-mode bypass` 或 `--dangerously-skip-permissions`（显式 deny 仍生效）。 |
| `--max-turns <n>` | 限制单轮最大模型迭代次数，防止失控循环。 |
| `--resume <ID>` / `-c` | 恢复指定会话或最近会话继续执行。 |
| `--data-dir <dir>` | 隔离测试/实验状态，避免污染真实会话与凭据。 |
| `--record-requests` | 记录完整原始 HTTP 请求与响应（见第 5 节）。 |

**实用经验**：

- **无 stdin 输入时务必加 `< /dev/null`**：否则父进程保持管道打开时，`dcl` 会额外等待 3 秒才开始。
- **长任务提示词走文件重定向 `< task.md`**：比命令行参数更安全，避免 shell 引号转义和长度限制。
- **日志分流**：`stdout` 承载结果/NDJSON 事件流，`stderr` 承载引擎诊断日志（`INFO`/`WARN`/`ERROR`），建议分别重定向到 `run.jsonl` 和 `run.err`。
- **源码验证用构建产物**：验证本地改动时直接运行 `./target/debug/dcl`（Windows 为 `dcl.exe`）或 `./target/release/dcl`，不要误跑 PATH 中已安装的旧版本。

---

## 2. 在执行过程中向 dcl 发送消息与控制

不要在另一个进程直接 `--resume <正在运行的ID>` 去提交新 prompt（同一会话受单执行者保护，会进入只读观察或拒绝提交）。运行中引导或介入应按场景选下面三种方式：

### 2.1 SDK 双向 `stream-json`（最适合程序/上游代理实时引导）

启动时同时开启双向 JSON 流并保持 stdin 打开：

```bash
dcl -C /path/to/repo \
  --input-format stream-json --output-format stream-json \
  --permission-mode bypass
```

1. **回合进行中追加引导消息**：向 stdin 写入一行 NDJSON：

   ```json
   {"type":"user","message":{"role":"user","content":"不要跑全量 core 测试，先只跑定向单测。"}}
   ```

   `dcl` 不会粗暴打断正在执行的工具，而是将消息写入持久队列，在**下一个安全工具边界**（当前工具批次执行完毕、下一次模型请求前）自动交付给模型，并在 stdout 输出 `{"type":"queued_input",...}`。若当前回合已准备结束，消息会作为下一轮输入继续执行，不会丢失。

2. **回合进行中发送控制指令**：向 stdin 写入 `control_request`：
   - 中断当前回合：`{"type":"control_request","request_id":"r1","request":{"subtype":"interrupt"}}`
   - 取消未交付的排队消息：`{"type":"control_request","request_id":"r2","request":{"subtype":"cancel_async_message","message_uuid":"..."}}`
   - 实时改权限模式：`{"type":"control_request","request_id":"r3","request":{"subtype":"set_permission_mode","mode":"plan"}}`
   - 查询上下文占用：`{"type":"control_request","request_id":"r4","request":{"subtype":"get_context_usage"}}`

### 2.2 跨会话 `SendMessage`（同项目、同数据目录）

当目标 `dcl` 已独立运行、没有挂载 stdin 管道时，可从另一个同数据目录会话调用 `SendMessage` 工具（`send`，`to` 填目标 `session_id`）：

- 目标正在执行回合 → `delivery: "next_safe_model_boundary"`（下一工具边界注入上下文）；
- 目标在线但空闲 → `delivery: "next_recipient_turn"`；
- 目标已关闭 → `delivery: "next_resume"`。

注意：跨会话消息会被标记为“任务数据”，不能借此绕过权限审批或覆盖系统指令。

### 2.3 交互终端控制（远程直连 / PTY）

- **dcl 直连**：宿主开启 `/remote-control listen`（或 `dcl --rc listen --host 127.0.0.1 --port 0`），另一端用 `dcl rc connect <host:port> --token <token>` 共享同一输入框与审批界面。
- **终端自动化**：macOS/Linux 可用 `tmux send-keys`；Windows ConPTY 使用 `scripts/drive_dcl.py`（见 `references/pty-harness.md`）。

---

## 3. 审查 dcl 的执行步骤

### 3.1 实时观察

- **解析 `run.jsonl`（`--output-format stream-json --verbose`）**：
  - `system`（`subtype: init`）：确认 `session_id`、`cwd`、`model`、`tools`（当前常驻工具列表）。
  - `assistant` / `tool_start` / `tool_result`：实时查看每轮思考、工具调用参数、是否报错及工具返回。
  - `request_attempt`：发现网络重试（`retry_wait`）、内容后超时恢复（`post_content_retry`）与单次连接耗时（`elapsedMs`）。
  - `result` / `turn_timing`：查看 `num_turns`、`duration_ms`、`usage`、`total_cost_usd`、`permission_denials`、`stop_reason`。
- **查看引擎日志 `run.err`**：定位 HTTP 异常、MCP 启动失败、数据目录告警与配置加载问题。
- **只读观察运行中的会话**：在另一终端执行 `dcl --resume <ID>` 进入只读观察（查看已发布的文本和工具行，不抢占执行租约）；或用 `dcl sessions show <ID>` / `dcl sessions export <ID> --output session.jsonl` 导出当前已提交快照。

### 3.2 事后深度审计 `session.db`

每个会话的完整轨迹保存在 `<DATA_DIR>/data/sessions/<session_id>/session.db`（SQLite）。可直接运行本 skill 自带的 `scripts/inspect_dcl.py`（仅依赖 Python 标准库，内部自动复制到临时快照以只读打开，不会与运行中的 `dcl` 争锁）：

```bash
# 审计已完成或运行中的会话（耗时拆解、缓存命中、单工具轮次、cd/sleep 反模式、最慢工具）
python3 "~/.deepseek-code/skills/bundled/dc-docs/scripts/inspect_dcl.py" session <session_id> --top 10

# 实时或事后解析 --output-format stream-json --verbose 输出的 run.jsonl
python3 "~/.deepseek-code/skills/bundled/dc-docs/scripts/inspect_dcl.py" stream run.jsonl --tail 15
```

核心表结构：

| 表名 | 关键字段与用途 |
| --- | --- |
| `messages` | `role`、`content`、`created_at`：核对用户指令、最终交付与后台 `<task-notification>`。 |
| `model_requests` | `request_number`、`model`、`effort`、`total_duration_ms`、`ttft_ms`、`generation_ms`、`throughput_tokens_per_second`、`input_tokens`、`cache_read_tokens`、`output_tokens`、`reasoning_tokens`、`finish_reason`、`response_blocks_json`。 |
| `tool_calls` + `tool_call_timings` | `name`、`input`、`output`、`is_error`、`duration_ms`：统计各类工具次数、耗时排行、错误率与命令内容。 |
| `execution_turns` | 回合起止时间与终态（`completed` / `cancelled` / `interrupted` / `failed`）。 |

**常用审计清单（定位“为什么慢 / 为什么跑偏”）**：

1. **拆解总耗时**：对比 `SUM(model_requests.total_duration_ms)`（含 `ttft_ms` 与 `generation_ms`）与 `SUM(tool_call_timings.duration_ms)`，区分瓶颈在模型、网络还是本地命令。
2. **检查 Prompt Cache 命中率**：`SUM(cache_read_tokens) / SUM(input_tokens)`；重点筛查 `input_tokens > 10000` 且命中率 `< 50%` 的请求，定位是系统提示/工具列表抖动还是上游缓存失效。
3. **检查工具批处理与 Shell 反模式**：
   - 统计每轮 `model_requests` 中仅包含 1 次工具调用的占比（单工具串行轮次过高会放大 TTFT 延迟）；
   - 检查 `Bash` 中是否存在**冗余 `cd`**、**前台 `sleep` 轮询后台任务**、或大量用 `sed`/`grep`/`cat` 替代内置 `Read`/`Grep`/`Glob`；
   - 检查 `Edit` 是否对同一文件拆成了过多串行小改（未合并或未使用 `MultiEdit`）。
4. **核对实际产出**：回到目标 worktree 检查 `git status`、`git log --stat`、`git diff` 与测试命令原始输出，避免仅凭模型口头汇报判断完成度。

---

## 4. `DEEPSEEK_CODE_DATA_DIR` 默认路径与目录布局（v2）

### 4.1 路径解析优先级

按顺序命中即生效（`dcl` 与 Desktop 共用）：

1. 启动参数 `--data-dir <目录>`（仅本次生效）；
2. 环境变量 `DEEPSEEK_CODE_DATA_DIR`；
3. 默认路径：
   - **macOS / Linux**：`~/.deepseek-code`
   - **Windows**：`%USERPROFILE%\.deepseek-code`

交互模式中可用 `/status` 查看 `Data directory` 确认当前实际路径。

### 4.2 v2 目录布局

根目录仅允许固定锚点与 7 个按需创建的分类目录（见 `crates/core/src/data_layout.rs`）：

| 路径 | 职责与关键子项 |
| --- | --- |
| `global.db`（及 `-wal`/`-shm`） | 全局 SQLite 目录：会话清单、项目映射、共享设置与跨会话消息队列。 |
| `layout.json` / `README.md` | 布局版本（`version: 2`）与迁移状态；勿手动编辑或删除。 |
| `AGENTS.md`、`CLAUDE.md` | 用户级全局指令文件。 |
| `skills/` | 个人技能；内置随附技能位于 `skills/bundled/<name>/`。 |
| `mcp-servers.json` | 全局 MCP 服务器配置（附 `mcp-servers.lock`）。 |
| `config/` | 用户与提供商配置：`config.json`（含提供商与 Key）、`settings.json`（hooks、requestRecording、沙箱等）、`providers.json`、`customization.json`（工具常驻/按需开关）、`skill-states.json`、`mcp-states.json`、`agent-models.json`、`startup-prompts.json`、`keybindings.json`、`remote-control.json`、`orchestrator.toml`、`backups/`。 |
| `data/` | 持久化业务数据：`sessions/<session_id>/session.db`（会话库）、`session-tasks/<session_id>.sqlite3`（会话任务库）、`global-blobs/`、`workspaces/`、`pasted-images/`、`memory.sqlite3`、`memory/`、`teams/`、`agent-messages.sqlite3`、`schedules.sqlite3`、`costs/`、`backups/`。 |
| `state/` | 运行时状态与锁：`shell-cwd/<session_id>`（前台 Shell 持久化目录）、`locks/session/`（会话执行租约等锁）、`presence/`（在线状态）、`workers/`（持久化 Worker 元数据）、`turn-completions/`、`session-env/`、`hook-once/`、`plugin-runs/`、`skill-runs/`。 |
| `tasks/` | 可恢复任务与运行产物：`background/<job_id>/`（`state.json`、`output.jsonl`、`stderr.log`、`detached`、`cancel`）、`shell/<session_id>/`（后台 Shell 日志）、`detached-shells/`、`turn-context/<session_id>.jsonl`、`checkpoints/`、`plans/`、`workflows/`、`super-agent/`、`toolscript-jobs/`、`wakeups/`。 |
| `cache/` | 可重建缓存：`environment/`、`mcp-tool-snapshots/`、`tool-outputs/`、`tool-output-metadata/`、`git-status/`、`file-state/`、`compactions/`、`screenshots/`、`plugins/`、`updates/`。 |
| `diagnostics/` | 诊断与抓包：`request-records/<session_id>/`（原始大模型 HTTP 请求与响应）、`logs/request-attempts/<session_id>.jsonl`（重试与耗时日志）、`debug/`、`hook-transcripts/`、`design-conversions/`。 |
| `resources/` | 托管安装的外部资源：`lsp/`（`/lsp install` 语言服务器）、`linux/rootfs/`。 |

---

## 5. 分析 `dc`（dcl / dcd）向大模型发送的请求

### 5.1 开启请求记录

默认关闭，生效优先级从高到低：

1. CLI 参数：`dcl --record-requests ...`（最高优先级，强制开启）；
2. 环境变量：`DEEPSEEK_CODE_RECORD_REQUESTS=1`（或 `true`；`0`/`false` 强制关闭；变更后需重启进程/Worker）；
3. 配置文件：`<DATA_DIR>/config/settings.json` 中设置 `{"requestRecording": {"enabled": true}}`（只读数据目录配置，不读项目级配置；`dcl` 与 `dcd` 共用）；
4. 以上均未指定时：`--full-access` / `--yolo` 会自动开启，普通启动关闭。

> **隐私提示**：请求体与响应正文**不脱敏**（包含对话、文件内容、工具输出等），仅移除认证头（`Authorization`、`Cookie`、API Key 等）。分享前必须检查内容。目录连续 14 天无修改时会自动清理。

### 5.2 快速查看与磁盘文件说明

```bash
dcl sessions list           # 获取完整会话 ID
dcl requests <session_id>   # 列出全部请求序号、purpose、model、HTTP 状态、完成标志及文件路径
```

所有文件位于 `<DATA_DIR>/diagnostics/request-records/<session_id>/`，同一次 HTTP 尝试共用前缀 `NNNN-<request_id>-aK`（`NNNN` 为会话内递增序号，`K` 为尝试次数，重试依次为 `a2`、`a3`）：

| 文件 | 内容与用途 |
| --- | --- |
| `.request.json` | 发给大模型的**逐字节原始请求体**（system prompt、reminders、tools schema、messages、effort/thinking、max_tokens 等）。 |
| `.headers.json` | 脱敏后的请求头，含关联头 `x-dcl-request-kind`（`turn`/`compaction`/`web-search`）、`x-dcl-session-id`、`x-parent-session-id`、`x-dcl-turn-id`、`x-request-id`、`x-dcl-retry-count`。 |
| `.sse.txt` | 流式响应的原始 SSE 字节流（按接收顺序保留全部 chunk）。 |
| `.response.json` | 非流式响应或 HTTP 错误响应原始正文（可能是纯文本/HTML）。 |
| `.meta.json` | `purpose`、`provider`、`model`、`http_status`、`started_at`、`ended_at`、`stream_completed`、`error`、`recording_error`。 |
| `compaction-NNNN.json` | 上下文压缩元数据（触发原因、压缩前消息与 token 估算、摘要、压缩后边界及关联的 `request_sequences`）。 |

配套文件 `<DATA_DIR>/diagnostics/logs/request-attempts/<session_id>.jsonl` 按时间线记录每次请求的 `elapsedMs`、`retries`、`waitMs`、`outcome` 与 `failure`。

### 5.3 典型排查手法

可直接用本 skill 自带的 `scripts/inspect_dcl.py requests` 辅助排查：

```bash
# 列出该会话所有 HTTP 请求尝试、状态、耗时、字节数与压缩文件
python3 "~/.deepseek-code/skills/bundled/dc-docs/scripts/inspect_dcl.py" requests <session_id>

# 检查第 1 次请求实际发给模型的 model / reasoning / system 长度 / 工具名称列表
python3 "~/.deepseek-code/skills/bundled/dc-docs/scripts/inspect_dcl.py" requests <session_id> --show-tools 1

# 对比相邻两次请求的 system / tools / messages 前缀差异，定位 prompt cache 失效点
python3 "~/.deepseek-code/skills/bundled/dc-docs/scripts/inspect_dcl.py" requests <session_id> --prefix-diff 1 2
```

1. **验证 System Prompt、Reminder 与工具 Schema**：用 `--show-tools 1` 或直接检查 `0001-*.request.json` 中的 `system` / `messages[0]` 与 `tools` 数组，确认常驻工具、MCP 工具或 skill reminder 是否按预期注入。
2. **定位 Prompt Cache 失效**：用 `--prefix-diff A B` 对比相邻两次 `.request.json` 的 `tools`、`system` 与 `messages` 前缀，找出导致前缀变化的字段。
3. **排查 4xx/5xx、截断与重试**：先跑 `dcl requests <session_id>` 或 `inspect_dcl.py requests <session_id>` 找异常序号，再对照 `.meta.json`（确认 `stream_completed` 与 `recording_error`，HTTP 200 不等于流完整）、`.response.json`/`.sse.txt` 以及 `request-attempts/<session_id>.jsonl`。
4. **排查上下文压缩问题**：打开 `compaction-NNNN.json`，沿 `request_sequences` 核对分段摘要与合并请求。

---

## 6. 开发构建、测试分层与 REPL 面板

### 6.1 构建、安装与测试分层

```bash
cargo build --locked -p deepseek-code-cli --bin dcl
./target/debug/dcl --version   # Windows: ./target/debug/dcl.exe --version

# Release 构建
cargo build --release --locked -p deepseek-code-cli --bin dcl
```

**安装路径约定**：

- **临时编译安装（开发/灰度验证）**：复制到 `~/bin/dcl{N}`，`N` 从 `1` 开始取当前已有最大编号 + 1 递增（例如已有 `~/bin/dcl7` 时安装为 `~/bin/dcl8`），不覆盖正在运行的旧版本。
- **正式安装**：安装到 `/usr/local/bin/dcl`。

| 要证明的结论 | 运行 |
| --- | --- |
| 核心解析、状态、发现 | `cargo test --locked -p deepseek-code-core --lib <filter>` |
| CLI 格式化、状态文件、斜杠派发 | `cargo test --locked -p deepseek-code-cli --bin dcl <filter>` |
| 真实进程、HTTP 形状、会话 DB | `cargo test --locked -p deepseek-code-cli --test <file> <filter>` |
| REPL 提示、面板渲染、按键处理 | PTY harness（`scripts/drive_dcl.py` / `references/pty-harness.md`） |

修改 `crates/cli` 或 `crates/core` 前先读 `docs/cli-regression-tests.md`；修改 `src/` 或 `src-tauri/` 前先读 `docs/desktop-regression-tests.md`。测试隔离统一使用 `--data-dir <temp>` + 本地 mock 端点，不碰真实会话与凭据。

### 6.2 REPL 面板与等价非终端命令

`/skills`、`/plugins`、`/tools` 打开整页面板：

- `←/→` 切标签，`Space` 切换启用状态，`Tab` 切换 on-demand（仅 Tools → Built-in），`Enter` 打开详情或状态选择，`Esc` 关闭。
- 非终端等价命令：`dcl skills list`、`dcl config show`、`dcl plugin list`、`dcl mcp list`（`dcl skills list` 输出完整正文，用 Python 或 `jq` 汇总）。
- 管道 stdin 不能启动 REPL（会报 `repl requires interactive stdin`），需使用 `scripts/drive_dcl.py` 或 `tmux`。

### 6.3 容易误判的测试失败与汇报规范

- **新数据目录未就绪**：会先弹提供商 onboarding 和文件夹信任提示；需预置 `<data>/config/config.json` 与 `<data>/config/startup-prompts.json`（见 `references/pty-harness.md`）。
- **基线环境失败**：Windows 路径断言/哈希或 macOS 并发 SQLite 锁（`SQLITE_BUSY`）可能是环境/基线问题；先单独重跑精确 filter，或在干净基线复现后再归因。
- **TUI 输出走 `stderr`**：PTY 测试需同时捕获 stdout/stderr，并用 `pyte` 渲染后断言屏幕内容。
- **汇报要求**：给出精确命令、filter、测试数量与结果，保持 unit / binary / integration / PTY 分层清晰，不以 unit 通过替代 PTY 或真实请求验证。

---

## 7. MCP 服务器

### 7.1 基本原则

- 全局配置文件：`$DEEPSEEK_CODE_DATA_DIR/mcp-servers.json`（默认 `~/.deepseek-code/mcp-servers.json`），`dcl` 与 Desktop 共用。
- 优先使用 `dcl mcp ...` 修改；仅在需要自定义 `cwd`、`env`、`headers` 或批量编辑时手写 JSON。
- `dcl mcp add` 只写配置不启动进程；运行中的会话不会热加载配置，修改后需新开会话。
- 工具名格式为 `mcp__<server>__<tool>`（`[A-Za-z0-9_-]` 以外字符替换为 `_`）。
- `dcl doctor` 不是 CLI 子命令；`/doctor` 仅在 REPL 内可用。

### 7.2 安装与配置

```bash
# stdio 服务器（Windows 路径用正斜杠或转义反斜杠）
dcl mcp add <name> <command> [args...]
dcl mcp add aoci D:/bin/aoci.exe mcp

# 远程服务器（支持 http / streamable-http / sse / ws / websocket，-H 仅用于远程）
dcl mcp add --transport http <name> <url>
dcl mcp add --transport sse  <name> <url>
dcl mcp add --transport ws   <name> <url>
dcl mcp add --transport http -H "Authorization: Bearer $TOKEN" <name> <url>
```

手动编辑 `mcp-servers.json` 示例（可选 `"env"`、`"cwd"`、`"type"`、`"url"`、`"headers"`，`"enabled"` 缺省为 `true`）：

```json
{
  "mcpServers": {
    "aoci": {
      "command": "D:/bin/aoci.exe",
      "args": ["mcp"],
      "enabled": true
    }
  }
}
```

### 7.3 工具状态与验证

```bash
dcl mcp state                  # 列出所有服务器状态（写入 config/mcp-states.json）
dcl mcp state aoci on-demand   # 设置状态：user | on-demand | user-only | off
dcl mcp list                   # 查看配置（独立进程中 running 通常为 false，属正常现象）
dcl mcp hosts                  # 查看存活 host 及正在运行的服务器 ID
```

| 状态 | 行为 |
| --- | --- |
| `user`（默认） | 工具进入每次模型请求。 |
| `on-demand` | 不进入初始 schema；在 “Tools you can load with ToolLoad” 中列出，先 `ToolLoad` 后调用。 |
| `user-only` | 对模型隐藏，模型调用被拒绝。 |
| `off` | 禁用并停止服务器；重新启用时恢复原状态。 |

新开会话后执行 `/mcp` 确认连接状态（如 `aoci  connected · 9 tools`）。需要直接验证 stdio 握手时使用本 skill 的探针脚本（仅依赖 Python 标准库）：

```bash
python "~/.deepseek-code/skills/bundled/dc-docs/scripts/probe_mcp.py" \
  --command D:/bin/aoci.exe --arg mcp

python "~/.deepseek-code/skills/bundled/dc-docs/scripts/probe_mcp.py" \
  --command D:/bin/aoci.exe --arg mcp --call aoci_header
```

### 7.4 `aoci` 示例与常见排障

- **不要在全局配置里写死 `--repo`**：`aoci` 默认按 cwd 定位仓库，`dcl` 用 `-C` 与 worktree 切换会自动更新 stdio 子进程 cwd；只有确实要固定单仓库时才传 `--repo` 或 `"cwd"`。
- 返回 `not_initialized` 时，在目标仓库根目录执行 `aoci init` 或检查当前 cwd。
- 运行中看不到新服务器 → 重启会话；`/mcp` 显示 failed → 检查 `command`/`args`/`cwd`/权限并用 `probe_mcp.py` 复现；`env`/`headers` 含敏感信息，切勿将 `mcp-servers.json` 提交到仓库。
