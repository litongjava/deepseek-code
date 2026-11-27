# dcl / dcd 项目记忆文件设计

状态：第 2 轮仅实现 P1 独立文件模块，尚未切换任何产品调用链。D1–D7 已由用户全部确认按建议执行。基线：`feat/memory-files` / `713a1fc7b8389f25dcf4771432a60e89e11bdb1b`。本文中的配置项、命令扩展、模块名和测试名如标注“拟新增”，均不是现有能力。

任务记录与源码证据：`/home/dev/code/dev-support/deepseek-code/memory-files-20261113/`。第 1 轮仅调研；第 2 轮仅新增独立 core 模块及必要依赖/测试文档，不访问真实用户记忆、不运行迁移、不进行 Git 写操作。日期目录遵循任务指定值。

## M3 当前模型：常驻索引（2026-11-22）

此节取代下文早期方案里的自动正文召回。普通用户消息不触发文件选择，不附加记忆正文，也不写逐消息召回凭据。模型仍可显式读取和写入记忆文件。

按下列顺序加载各目录的 `MEMORY.md`：`<data>/memory/global/`、可选的 `memory.team_dir`、项目的 `<data>/memory/projects/<scope>/memory/`、仅当前子代理的同级 `agents/<agent-name>/`、当前会话的 `<data>/sessions/<key>/memory/`。项目沿用现有绑定和私有 scope 规则；子会话不读取父会话的 session 索引。fork 复制 session 目录，删除会话一并删除它。

非空索引各带 scope 和路径，在冻结的第一条用户消息中紧随项目指令；每份最多 200 行、25 KiB 原始字节，截断时指出完整文件。后续请求只为变化的 scope 合并发送一条提醒，不改写首条消息。压缩边界保存当时的各 scope 快照；重放只呈现这份快照一次，不再发送边界前的提醒。恢复时即使磁盘又有改动，也不以实时文件重建历史请求。

## 1. 原始方案的范围与依据（历史）

目标是以项目 Markdown 文件作为本地长期记忆的唯一事实源：一个文件一条记忆，`MEMORY.md` 为启动索引；引擎按当前对话自动附加相关正文；模型通过普通文件工具维护事实。SQLite 可以继续承载会话、服务状态及可删除重建的搜索缓存，但不能继续作为本地记忆正文的另一份权威存储。

用户已经确定的四类 `metadata.type`、Why / How to apply、`[[name]]`、自动相关性召回、停用本地专用记忆工具及无损迁移，均为本设计要求。不要把“对齐”解释成照搬另一个产品的全部实现。

参考核验：[Claude Code 官方 memory 文档](https://code.claude.com/docs/en/memory#auto-memory)描述了按仓库共享 worktree 的记忆目录、逐条文件与启动索引，以及索引前 200 行或 25KB 的加载上限；其公开文档描述正文由普通文件工具按需读取。本项目按用户要求增加引擎自动正文召回。本文的字段规范、权限和事务方案是本项目设计，不声称是 Claude Code 的内部实现。

已读取仓库约定、`docs/cli-regression-tests.md`、`docs/desktop-regression-tests.md`，并查阅 `docs/cli-remaining-work.md`。后者的历史路径、提交要求与本轮指令冲突时，以本轮工作树、证据目录和禁止 Git 写入为准。

## 2. 现状

### 2.1 调研方法与限制

按要求先调用 `mcp__codegraph__codegraph_explore`，查询 memory 模块及相关符号。虽然 shell 中 `test -d .codegraph` 成功，MCP 返回“未索引，找不到 .codegraph”，并要求本会话不再调用；`command -v codegraph` 返回 1。未修改索引、未切到主检出；后续以 `rg`、带范围的源码读取和只读 `git show fab4608` 追踪。以下是源码核验，不是 CodeGraph 成功输出，也不是运行时验收。

源码窗口与完整文件 SHA-256 分别保存在任务目录的 `source-evidence.md`、`source-sha256.txt`。行号均对应上述基线。

### 2.2 存储、配置、HTTP 与压缩

| 位置 | 实际行为及影响 |
| --- | --- |
| `crates/core/src/memory/mod.rs:30` | `MemoryItem` 为 id、content、importance、created_at、可选字符串 metadata、similarity_score；没有结构化项目字段、name、description 或 Markdown 文件索引。 |
| `memory/local.rs:11` | `memory.sqlite3` 内 `memories(id PRIMARY KEY, content UNIQUE, importance, created_at, metadata)`，另有 settings、migrations、dream_sources、dream_runs。busy timeout 为 5 秒。没有 project_id 列或项目过滤。 |
| `memory/local.rs:36` | 相同正文 ingest 更新最大 importance 并替换 metadata，保留原 ID/创建时间。update 只改正文和重要性；clear 清 memories 与 dream 状态，保留配置。 |
| `memory/local.rs:43`、`:97` | 本地搜索每次读全表，大小写归一化、英文词和中文 bigram；整句包含为 1，否则命中词数/查询词数；按相关性、importance、时间排序。不是向量搜索；空 query 按创建时间列出。 |
| `memory/mod.rs:67`、`:104` | 默认 enabled/auto_ingest/auto_search=true，backend_url=local，top_k=5，importance 阈值 0.3，压缩阈值 2,500,000 tokens。配置存在同一 SQLite，其他进程每次 get_config 重读；构造失败会回落默认。 |
| `memory/mod.rs:155` | 远程分支调用 `/health`、POST `/search`、POST `/ingest`、GET `/stats`、POST `/clear`，HTTP client 总超时 5 秒；此请求结构没有项目隔离字段，也没有独立鉴权配置字段。编辑/删除单条、导入导出及 dream 只支持 local。 |
| `memory/mod.rs:178` | ingest trim 正文，最多 32,000 字符，importance 必须有限且在 0..1，低于阈值拒绝。显式 ingest 不调用自动摄取的凭据检测。 |
| `config/mod.rs:61`、`:113`、`:220` | 另有 OrchestrationConfig 的 memex_backend_url，默认 localhost:8765；本轮搜索该 getter 没有找到生产调用点。不能将它等同于当前 engine/Bridge 使用的 MemoryConfig。 |
| `memory/caveman_rtk.rs:64` | CavemanRTKSystem 是内存中的 segments / token 权重 / feedback；按分词重合检索和规则打分/衰减，embedding 字段可选但当前不生成语义向量。它不是 SQLite 的持久检索索引。Bridge 有独立 Caveman 统计/添加/查询入口。 |
| `memory/mod.rs:269`、`:576`、`:623` | TieredCompressor 是上下文压缩辅助；仍有压缩消息提示“用 memory_search 找细节”。切换文件后必须一起修改，不能只改 memory.md。 |

### 2.3 召回、注入、写入与委派

`native_engine/engine_core.rs:1397` 总是以 workspaces 父目录的 `memory.sqlite3` 构造 client，再调用 `tool_loop.rs:3006` 的 `with_memex` 建立 ContextManager。Code/Desktop 两种 prompt profile 共用这条路径，没有按项目打开独立记忆。

`memory/mod.rs:733` 的 `before_api_call` 用当前用户消息搜索一次，再提取至多 5 个文件名/“之前”等关键词逐个补搜，每个 top 3；按 ID 去重后截为配置 top_k。正文每条截前 2,000 字符，包装为标明不可信历史数据的 system-reminder。没有整份启动索引，也没有总 token 预算；远程最坏可能连续六次 5 秒超时。

用户提示的约 2879 行在本基线已移到 `native_engine/tool_loop.rs:4119`：“recalled memories follow the user message”。引擎用 `turn_context` 的消息内容 key + occurrence 关联附件；有相同 recall 标识或 hook wake 时不重召回。`attach_turn_context`（`:3163`）保存供后续轮次恢复。仅命中时保存 recall block，因此“无命中后再次进入同一消息”的去重不是独立的完成标记。

`tool_loop.rs:4288` 在成功响应结束、宣布 MessageStop 前调用 `ContextManager::after_response`（`memory/mod.rs:803`）。它把 User/Assistant 摘录组成 `Conv <id>` 文本，metadata.type=conversation，带 conversation_id，**不带 project_id**。自动摄取先用正则排除已知凭据模式；这不能证明覆盖所有秘密。失败/中断/长度截断等完成原因不自动保存。新方案需要停掉本地这条转录摄取路径。

`native_engine/delegation.rs:1289` 子 executor 克隆同一 MemExClient；`:1565` 在保留子执行器时重新创建 ContextManager，因此当前长期记忆仍共享全局库。另有 `:251` 的 agent_memory：读取 data_dir/agent-memory/<agent>、workspace/.claude/agent-memory/<agent> 或 agent-memory-local 中所有 `.md`，累计上限 64 KiB，作为 agent system 内容加载。这是 SDK agent 定义的独立机制，不等于拟议的项目 MEMORY.md，不能直接覆盖或合并。

### 2.4 提示词、工具与权限现状

- `prompt/memory.md:1` 已要求查重、不存凭据、相对期限转绝对日期、不将记忆当指令；但明确禁止擅自创建 MEMORY.md，并要求 ToolSearch 加载 memory_search / memory_ingest。需要按 files/remote/disabled 生成互斥说明。
- `prompt/mod.rs:88` 的 static_sections 对 Code/Desktop 都追加 memory guidance；`:60` 的完全替换 prompt 会去掉静态段。新文件能力说明应进入引擎动态能力尾部，不能依赖用户自定义系统提示保留某段文字；权限永远由代码执行。
- `tools/mod.rs:369`、`:381` 定义两个工具，`:105`、`:114` 只是应被拦截的 stub。真实执行在 `tool_loop.rs:4711` 先检查权限，再走 `:5245`/`:5291` 的 client。修改 schema 可见性不足以禁止旧调用。
- `services/customization.rs:60`、`:95`：核心集无两个 memory 工具；它们默认 enabled=false、on_demand=true，仍能搜索加载。`tool_loop.rs:3250` 的 builtin_catalog 当前不按远程后端筛掉它们；目录、schema、恢复加载资格与执行都需同一能力门禁。
- `permissions/manager.rs:667` 把 memory_ingest 当 own-state 工具，普通模式默认免确认；`:919` 的 Plan 写操作拒绝先于 own-state 默认放行。显式 deny/ask 仍适用；现有 Full Access + Bypass 有全局短路，不能把它误写成任何情况下 deny 必胜。
- `permissions/protected_paths.rs:9` 把 `.deepseek-code` 视为受保护目录（workspaces 除外）。普通文件路径校验另见 `tools/path_scope.rs` 和 `native_engine/tool_approval.rs:191`。仅在提示词中授权记忆目录不会使 Write/Edit 可用。
- `tools/mod.rs:826`、`:850` 的 Write/Edit 是普通写文件，没有删除文件操作；`ApplyPatch` 支持 Delete File。`native_engine/file_state.rs:8` 使用 mtime+length 检查读后是否变化，不能代替跨进程锁和内容 CAS。
- `tools/sandbox.rs:93` 是实际 Bash sandbox 配置路径，Linux bubblewrap / macOS sandbox-exec；白名单默认 workspace、temp、额外写目录。普通 Rust 文件工具并不因此自动运行在该 OS sandbox 内。Router / Coder 额外门禁在 `tool_approval.rs:20`、`:69`，ToolScript 内层也必须沿用。

### 2.5 CLI、Desktop、Trajectory 与其他消费者

| 入口 | 当前数据链 |
| --- | --- |
| `crates/cli/src/memory.rs:27`、`:80` | `/memory` entries 与 `dcl memory` 直接开 data_dir/memory.sqlite3，支持 JSON CRUD、clear --yes、导入预览/备份/导出、dream。列表和 CLI 按 ID 查找目前最多 100 条，不能复用这个路径做全量迁移。 |
| `crates/cli/src/display_commands.rs:301`、`:436` | `/memory` 还编辑 data_dir/CLAUDE.md、workspace/CLAUDE.md、CLAUDE.local.md，`show` 打印已加载说明。`memory_shortcut.rs:1` 的 `# note` 写项目 AGENTS.md/CLAUDE.md；这些功能与长期事实记忆分开保留。 |
| `src-tauri/src/bridge/mod.rs:291`、`:2410` | AppState 持有全局 MemExClient、ContextManager；没有请求级项目 scope。 |
| `src-tauri/src/bridge/routes/memory.rs:6` | `/api/memory/search|ingest|stats|config|clear|export|import|items/{id}`；update/delete 走同一 client。 |
| `src/api/memory.ts:3`、`src/components/MemoryPanel.tsx:29` | 直接消费 MemoryItem 和 MemoryConfig，支持统计、查询、内容/importance 编辑、删除确认、JSON 导入预览/导出，设置包括自动保存完成对话和远程 URL。UI 当前没有 project selector、name/description/type 或冲突版本。 |
| 提交 `fab4608` | `feat: manage memories, the system prompt and tools`，23 文件；增加 CLI entries 管理、单条更新删除、Bridge/API/UI 和 customization。只读 stat 保存在 baseline-and-history.txt；当前代码已含后续变动，不能把该提交当整个现状。 |
| `src/features/trajectory/model.ts:12`、`:136`、`:325` | Trajectory 消费请求快照 systemPrompt/systemMessages/tools 与持久消息，不直接读 memoryApi/SQLite。`tool_loop.rs:5507`/`:6272` 的快照只摘取 system-role conversation messages；贴在 user 上的 recall 不应未经验证就宣称已在当前轨迹详情完整展示。`src-tauri/tests/trajectory_persistence.rs:8` 仅证明注入上下文样例快照可持久化。 |
| `services/dream.rs:47`、`src-tauri/src/commands/peripheral.rs:297` | CLI/Desktop 后台 dream 共用服务与同一库；不仅是 MemoryPanel 入口需要改。 |
| `services/magic_docs.rs:278` | 自动文档更新虽然选定项目，仍按文档标题从全局记忆查 top 5，再传给项目上下文。必须迁移为同项目 provider。 |
| `orchestration/mod.rs:256`、`:1078`、`:1118` | 可选 shared_memory 注入与任务结果摄取；本轮未找到 with_shared_memory 的生产装配调用，记录为潜在消费者，不当成已启用链路。 |

### 2.6 项目身份、dream 和导入导出

`runtime.rs:218` 的 CLI 会话创建将 workspace canonicalize，复用同路径、单目录且受信任的 project，否则创建 UUID；conversation 保存 project_id。`db/project_repo.rs:159` 管理有序多目录和信任，首目录是 workspace_path。`native_engine/workspace.rs:142`/`:171` 从持久 project/trust 与受管 worktree 绑定解析真正工作目录；Home 会话没有项目时使用 data_dir/workspaces/<session>，不能由调用者任意覆盖。`project_id`、原始项目目录、实际 worktree cwd 是不同概念；现有记忆完全没有利用它们隔离。

`memory/transfer.rs:22` 接受 version=1 MemoryArchive 或旧 MemoryEntry 数组；旧条目整体 JSON 存入 metadata.legacy_source。导入验证 id、内容、importance 和 RFC3339 时间，实际写前用 VACUUM INTO 做库备份，再在 Immediate 事务检查 ID/内容冲突：完全相同则 skip，不同则整批回滚；preview 回滚事务。导出走不受 enabled/top_k 限制的全量 local.search，但 metadata 解析错误会被 local.rs 默认为 None，且 archive 不含 settings/dream 表。因此它是可复用的兼容格式，**不能独自充当逐字节无损迁移备份**。

`memory/dream.rs:15` 以来源快照识别未合并/已变化条目，排除 dream 类型及已知凭据。`services/dream.rs:77` 的自动策略默认关闭，默认门槛 24 小时/5 条，一批最多 100 条、正文合计 48,000 bytes，模型生成纯事实摘要，租约心跳支持取消。`commit_dream`（`:72`）事务内重验配置/来源快照/未处理状态，新摘要 importance=0.9、metadata.type=dream，保存 source_ids 与运行记录，**不会删除原记录**。新模型不应机械地每轮再复制出一份摘要文件。

## 3. 核心架构与目录

拟新增共享 core 服务：`MemoryScopeResolver`、`FileMemoryStore`、`MemoryContextBuilder`、`MemoryMigration`、独立的 `RemoteMemoryProvider`。CLI、Bridge、engine、magic_docs、dream 使用同一服务；API 适配器不自己拼接路径。`MemExClient` 暂留作远程/旧库兼容适配，不再代表所有记忆。

```text
<data_dir>/memory/
  config.json                         # 后端、预算；模型不可写
  state/
    projects.json                     # 身份/别名映射；模型不可写
    <project-key>/                    # 锁、journal、generation、hash cache
  projects/<project-key>/memory/       # 当前会话唯一可免确认写的根
    MEMORY.md
    testing-feedback--<stable-id>.md
    release-context--<stable-id>.md
  legacy-unassigned/<migration-id>/    # 无归属旧条目，不自动注入
  quarantine/<migration-id>/           # 原始转录、异常或疑似凭据，不自动注入
  backups/<migration-id>/              # SQLite snapshot、原始行、清单
  trash/<project-key>/                 # 删除/替换前版本；模型无默认访问权
```

记忆与仓库文件分离；不写工作区 `.claude`，不读取真实 `~/.claude` 或 `~/.deepseek-code` 来迁移别的产品。data_dir 通过现有 RuntimePaths 注入，绝不在文件后端失效时回落系统 temp 或全局库。

### 3.1 项目键（推荐方案，决策 D1）

1. 从持久会话的受信任项目解析 scope，不能从模型的 file_path / project_id 参数声明权限。工具中 `cd` 不改变 scope；显式工作区迁移在回合边界重新解析。
2. 单 Git 仓库：以受信任根下通过只读 Git 查询得到的 canonical `--git-common-dir` 为身份，tag=`git-v1`。同仓库子目录、普通/受管 worktree 共用；不同 clone 即使远程 URL 相同也不共用。Git 查询禁用可选锁、不执行 hooks；失败应报身份不可确定，不能静默选另一目录。
3. 非 Git 项目：tag=`dir-v1` + canonical 主根原生路径。多根 Desktop 项目：tag=`roots-v1` + 去重、排序、长度前缀编码的各根身份集合；单次 --add-dir 不自动把别的仓库并入记忆。去重后仍有多个身份时作为组合 scope，与单根 scope 分离；同仓库多个根去重为单仓库身份。界面明确显示。
4. 无 project 的 Home 会话：tag=`private-v1` + 持久 private owner session ID，owned child 继承父 private scope；不建立隐式全局用户池。需要跨 Home 会话共享时由用户建立命名项目，或以后单独引入显式 user scope。
5. key=`v1-` + SHA-256(tag + NUL + 长度前缀身份字节) 的全部 64 位十六进制。Unix 使用原生路径字节，Windows 使用 canonical 最终路径的规范宽字符编码和卷大小写语义；不使用 lossy UTF-8 或一律 lowercase。显示名不参与路径计算。UNC/network 根首版不自动启用文件写后端，要求显式选本地可靠存储。
6. projects.json 将 project_id 映射到持久 key 和身份记录。路径移动/项目增删根需显式 rebind/迁移；无自动合并。若相同 hash 对应不同身份，拒绝而非覆盖。旧会话解析到不同身份时需迁移绑定，不能带着旧写权限继续运行。

目录键不是授权令牌。用户有完整应用管理权限可在 UI 管理多个项目；模型会话只拿当前 scope capability。解析 scope 时仍检查 project trust/归档状态。多目录选择、Windows 路径别名、符号链接的等价性必须跨平台测试，Linux 结果不推断 Windows 已支持。

## 4. 文件格式、索引与校验

以下为合成格式样例，不来自用户真实记忆。

```markdown
---
name: 测试前先说明验证范围
description: 用户要求修改执行链路时先说明验证层级，适用于 CLI 与 Desktop 的验收报告。
metadata:
  type: feedback
  id: 01-example-stable-id
  created_at: 2026-11-13T00:00:00Z
  updated_at: 2026-11-13T00:00:00Z
---
用户要求区分单元测试、本地 HTTP 集成和真实终端验收，不将一层结果视为另一层通过。

## Why
此前只跑单元测试的报告未能证明真实终端行为。

## How to apply
开始前列出验证场景；结束时分别列出已验证和未验证部分。

相关背景：[[验收环境约定]]
```

- UTF-8 Markdown，首个 YAML frontmatter 必含 name、description、metadata.type；type 仅 user / feedback / project / reference。类型不是跨项目权限：user 类型也只存当前 scope。
- name 为非空单行、最多 80 Unicode 字符；description 为非空一句话、最多 240 字符且不超过 512 UTF-8 bytes，描述适用情境。正文非空；feedback/project 必有 Why 和 How to apply。日期不能推断时写“日期未确定”，不能按迁移日期把“明天”擅改成确定事实。
- runtime 在首次提交补 stable id 和 UTC 创建/更新时间，后续保持 id/创建时间。建议文件名为小写 ASCII slug + stable id + `.md`；P1 也接受 `one.md` 等安全小写 ASCII 文件名（最多 180 bytes），stable id 保存在 frontmatter，与 name/文件名可独立变化；禁 `..`、路径分隔符、设备名、冒号/ADS、尾空格/点、保留 `MEMORY.md`。不允许文件名或 YAML 字段指定 scope。
- 普通文件上限 64 KiB，frontmatter 上限 8 KiB、嵌套深度 8；拒绝重复 YAML key、自定义 tag、alias/anchor 扩张、非预期根类型、非法日期和枚举。保留可解析的未知扩展字段，不由编辑器无意丢弃。旧记录超限走隔离与人工拆分，不截掉原文。
- name 以 NFC + Unicode case folding 做唯一性/链接 lookup；原字面用于显示。冲突不自动覆盖。`[[name]]` 只在同 scope 解引用，不执行 include、shell、HTTP 或跨项目路径；断链给诊断，不阻止读其他记忆。循环由 visited-id 集合阻断。
- 凭据检测覆盖已有规则并增加写入/注入前检查；疑似内容不入活动索引。检测是启发式，不能宣称发现所有凭据。新写入失败时保留现有文件；外部编辑的异常文件留在原处并标记不可召回，不能静默移动用户文件。

### 4.1 MEMORY.md 的维护职责

规范行：`- [测试前先说明验证范围](testing-feedback--<stable-id>.md) — 用户要求区分各验证层级，适用于验收报告。` 标题/description 来自各文件，链接为单层相对文件名，不支持 URL、绝对路径或 Markdown HTML。每条活动记忆恰好一行；详细事实不放索引。

模型先 Read 索引并查阅相关文件，再用 Write/Edit 改记忆和索引。core 把文件 frontmatter 作为条目事实源，在每次受管变更的同一项目事务中增删/修正对应索引行，保留仍合法的既有行顺序，新行按规范化 name/文件名排序附后。这是崩溃与遗漏兜底，不代替模型判断哪条事实该保留。模型单独编辑索引可以调序，但不能凭空创建记忆或只删索引来隐藏现存文件；不一致返回可修复诊断，删除需删正文文件。

外部编辑后，下一次使用前 reconcile 新增/删除/重命名；frontmatter 无效的文件不自动注入。修复索引不调用模型。含非索引正文的外部 MEMORY.md 先保留原版至备份，再提供明确修复结果；不将其长段文字提升成启动指令。`memory doctor/reindex` 拟作为 CLI 管理命令，非模型专用工具。

### 4.2 启动加载与预算

正常情况下，每个新会话的第一次模型请求包含**完整 MEMORY.md**，无论用户消息是否与它匹配；不能先相关性筛选索引。恢复、fork 和 compact 后由当前 scope 的稳定 context block 重建索引一次，记录 project-key、generation 和 content hash。scope 改变必须撤销旧权限并替换索引；旧轮次快照不重写。

默认索引硬上限取三者先到：200 行、25 KiB、4,096 tokens。80% 时向模型/面板提示精简；超限的有效写入仍落盘，返回 `saved=true,index_over_budget=true`，要求缩短，但不伪报“未保存”。加载保留原顺序的完整行前缀，优先给截断标记预留预算；标记包含 omitted count、完整索引路径和预算原因。不切半个 UTF-8 字符或链接，不丢磁盘内容；自动召回仍搜索全体合法文件，不限于索引可见前缀。正常全量、异常有界截断是“整份索引”和有限窗口的明确边界。

正文默认 top 5、总预算 4,096 tokens、每文件最多 1,200 tokens；优先给出完整小文件，否则按段落输出事实与 Why / How to apply 的标记摘录，带路径和 omitted 标识。不能为了装长文件排挤全部其他命中。预算包括 wrapper、frontmatter 展示和路径，来源 metadata 不整包塞给模型。

总记忆预算 `B=min(8192, floor(context_window*0.10), 当前请求可用输入余量)`；索引优先，剩余给正文，再由既有全请求预算/compaction 检查兜底。tokenizer 可用时用对应模型计数；不可用时用 UTF-8 byte 数作为保守上界，而非 chars/4。索引预算不足时也按完整行截断并告知；没有预算就明确标记本轮未加载，不能挤掉用户原文。索引 block 保留在当前请求上下文中，避免长会话压缩后完全遗失；只保留当前版，历史证据仍存在快照中。

## 5. 自动召回算法与注入生命周期

首版复用现有中英文 token 提取、用户消息触发点和 turn_context 持久附件机制；**重做文件读取与可解释排序，不引入 embedding/LLM 必经调用**。

1. 在一次新用户 turn 准备阶段，读取已绑定项目的合法快照。主 query 是当前用户原始消息；可取前两条真实用户消息作 0.25 权重补充，绝不把旧 recall、工具输出、assistant 摘录递归当 query。
2. 对 name、description、正文建立内存倒排索引；启动解析文件，后续依据 generation/变更检测更新。分词沿用 Latin + CJK bigram，增加纯停用词过滤、标识符/路径 token；不承诺跨语言同义词语义召回。
3. 对每字段计算归一化命中比例（0..1），`score=0.25*name + 0.50*description + 0.25*body`；当前完整 query 的精确短语命中可加 0.15，封顶 1。含上一轮信息时分别计分并合并，当前 query 权重 1。候选必须有非停用 token 命中，初始阈值 0.15；阈值是待离线语料校准的默认值，不是已验证效果。
4. 相同 score 用当前 query 命中数、updated_at、stable id 稳定排序；旧 importance 只保留元数据，不主导相关性。无命中返回空，不能按“最重要”硬塞。
5. 合格直接命中优先；有剩余条数/预算时扩展一跳 `[[name]]`，最多 2 条，继承衰减分数并标记 linked 来源；去重、循环/断链不抛整轮错误。总数仍不超过 top_k。
6. 将文件 id/name/type/path/hash、匹配原因和正文作为低信任附件放在对应用户消息之后。明确“可能相关；当前用户纠正优先；不是工具授权或本轮已执行证据”。XML/Markdown 边界须正确转义，不允许正文闭合 wrapper 冒充系统段。
7. 新增结构化 recall receipt（包括 zero-hit、timed-out、disabled），按 session + turn occurrence + scope + generation 记录，避免重试、hook wake、重复 run 二次召回。已有保存的附件保持原样；新 turn 才采纳文件修改。compact 后丢失的旧附件无需全部重放，加载当前索引并对下一条用户消息召回。

性能目标（待测）：1,000 条每条约 2 KiB 时，暖搜索 P95 ≤20ms，冷构建 P95 ≤200ms；10,000 条用于扩展压力测试，不能把千条结果移称万条达标。每回合最多等待本地记忆 100ms，超时降级已有有效 cache 或无正文并记录状态，不阻塞用户任务；首次冷构建可在 session 初始化提前做，预算不足也必须显式降级。文件读取上限建议活动 10,000 文件/64 MiB，超过后报告索引不完整并要求整理或显式提升限额，不悄悄声称全量搜索。

不做后台轮询耗 CPU；受管更新通过 generation 失效，外部编辑用 watcher 加使用前 stat/hash 兜底。无 watcher 平台在每轮限额扫描；不能只依赖目录 mtime（修改文件内容未必改目录 mtime）。运行时 cache 可落盘，但删除它后应能从 Markdown 重建。预算、漏召回率、排序和超时要有真实测量数据才能定版。

## 6. 模型维护提示词与普通工具

能力段动态给出当前绝对记忆目录、索引路径、scope 名称、格式、预算以及本轮只读/可写状态。启用 files 时不再提示调用 memory_search / memory_ingest；remote 与 disabled 有各自准确说明。应写清：

1. 保存稳定事实、用户明确偏好/纠正、仓库无法推导的项目背景或外部参考定位；不复制仓库已有代码/架构/命令，不存临时进度、整段对话、未经证实推断或工具执行日志。
2. 新增前读 MEMORY.md 和相关文件查重；同一事实更新原条目，矛盾以当前用户明确信息为准，无法判定则注明来源/时间并询问，不发明结论。过时就删除；不要用多个近似条目覆盖问题。
3. 每条文件必填 name/description/type；feedback/project 写事实、Why、How to apply。正文日期用有依据的绝对日期，模糊日期保留不确定性；禁止凭据、token、密码和私钥。
4. 索引一条一行，记忆链接用同项目 `[[name]]`；创建、改名、删除后维护索引。不要写 data_dir 配置、其他项目、锁/备份或把这里当代码执行目录。
5. Read 后才 Edit/覆盖 Write；遇到 Conflict 必须重新读再判断，不能盲目重试覆盖。工具确认成功才声称保存，disabled/只读/失败时如实说明；必要时把待保存内容留在当前回复，不偷偷排队持久化。

**删除能力缺口（决策 D3）**：现有 Read/Write/Edit 都不能 unlink；不能假装空文件等于删除。推荐保留它们负责新增修改，同时用现有普通 `ApplyPatch` 的 Delete File 完成精确删除，仍不添加专用 memory 工具。ApplyPatch 已支持删除，但需在记忆 scope 走下面相同事务与授权，多路径 patch 有一条超范围就不能获得记忆免确认；若用户要求严格只有三种工具，替代方案是给 Write 增加明确的删除参数（普通文件工具 API 变更），不采用空内容隐式删除。D3 已确认采用 ApplyPatch；工具适配在 P3 实施。

## 7. 精确权限与 sandbox 语义

### 7.1 能力范围与顺序

为当前会话派生 host-only `MemoryDirectoryCapability`，绑定数据目录、scope、canonical 根、policy revision 与允许动作。模型输入里的同名字段不可信。此能力只覆盖该根下单层合法 `.md` 记忆和 MEMORY.md；state/config/backups/trash/quarantine/别的项目均不在范围，目录本身不可 rename/unlink。

同一公共执行链用于直接调用、ToolScript、委派、恢复以及 hook 改写参数后的调用：检查工具加载/允许列表和角色门禁 → 检查当前模式与明确 deny/ask、hook 约束 → 安全解析全部目标 → 确认能力与本轮 scope/版本 → 应用精确免确认默认值 → 锁/CAS/提交前复验。未命中能力时走原有路径，不扩大通用 allowed_directories。目录 capability 可豁免该目录因 `.deepseek-code` 祖先产生的默认 protected-path 提问，不能豁免其他受保护文件或明确规则。

| 情况 | 文件记忆行为（推荐 D2） |
| --- | --- |
| Ask / AcceptEdits / Auto / DontAsk，合法当前 scope | Read 与受管 Write/Edit/精确删除默认免确认；无需模型分类器。DontAsk 无默认询问，但显式 ask 仍拒绝或遵循当前无交互策略，不能自动允许。 |
| Plan | 索引/正文可读，全部记忆写入/删除拒绝；不复用“计划文件”的特殊写权限。可将候选内容留在回复，退出 Plan 后重新核验再写。 |
| Router / 尚无当前计划批准的 Coder | 不增加角色例外；若既有门禁拒绝普通编辑，则记忆也只读。将来放宽必须单独验收，不靠记忆路径绕过任务计划门禁。 |
| 显式 deny / ask / disabled tool / agent allowlist / Hook deny | 继续生效；免确认只是默认权限。既有 Full Access + Bypass 的全局 override 不在本次重定义，但不能绕过文件后端的结构、路径与 CAS 校验。 |
| disabled 或其他 scope | 不创建例外；disabled 不加载新索引/召回，不主动保存；旧历史附件仍是历史，不能声称关闭功能会擦除过去已发送内容。 |
| Bash/PowerShell/MCP/custom tool | 无记忆目录免确认特权；不能用命令字符串前缀判定安全的 rm/cp/python。遵循原权限和 sandbox。 |

### 7.2 路径攻击与文件系统实现

- 在已验证的 root 目录句柄下逐组件解析，拒绝任何 symlink、Windows junction/reparse point、路径穿越、网络设备路径、ADS 和非普通文件；不能用字符串 starts_with。数据目录由用户选择的 canonical 根锚定，之后项目子链禁止重定向。
- 对已有目标检查硬链接数，nlink>1 拒绝免确认修改；不能仅凭 canonicalize 防住硬链接。创建采用 exclusive create，写入同目录临时文件后替换，永不跟随最终软链接，不以普通 truncate 打开外部 inode。
- Unix 通过 dirfd/openat 系列与 no-follow/beneath 限制；Windows 使用拒绝 reparse 的 handle 和文件身份验证。前置 canonicalize 后再按普通路径打开存在 TOCTOU，不能作为完成实现。目录句柄和目标身份在提交前复验；不支持可靠原语的平台先只读/明确报错，不静默降级成宽权限写。
- 本地目录建议 0700、文件 0600，Windows P1 继承 host 提供的数据目录 ACL，不自动创建/修复 DACL；宿主必须提供用户私有根，后续集成验收检查 ACL；备份可能含旧凭据，绝不通过模型 Read 的额外 scope 暴露。文件名、description 和 UI Markdown 也按不可信文本渲染，禁 HTML/远程自动加载。

### 7.3 Sandbox 边界

免确认是应用权限能力，不是 OS sandbox 绕过。Rust 文件工具经受管服务在宿主进程写精确根；已有 Bash sandbox 的可写目录列表**不新增整个 data_dir，也不自动新增 memory 根**。Bash 因此可能不能写记忆，模型应使用普通文件工具。用户主动额外授权 Bash 的行为沿用原语义，不宣称应用能阻止同 UID 的任意外部进程直接改文件。

若宿主自身受 OS 只读 sandbox，服务不能越界写；返回 MemoryWriteUnavailable，目录例外不能获得 OS 所无权限。后台/远程 Worker 必须通过宿主绑定解析相同 scope，不能使用提交者传来的任意绝对路径。

现有 Plan 允许未被危险规则拒绝的普通 shell 命令（`permissions/manager.rs:888`），所以这里的 Plan 只读承诺适用于受管记忆操作，不等于整个进程的文件系统只读。若产品要求连非 sandbox 的 Bash/外部编辑器都不能改变记忆，须另做 OS 隔离或文件服务隔离；本次不以目录白名单冒充这层保证。

## 8. 并发、崩溃恢复与外部编辑

每项目跨进程 OS 文件锁放在 `state/<key>/`；不要把 lock 文件放入模型可写目录。CLI、Desktop、engine、dream 共享协议；锁不依靠单进程 Mutex。等待可取消，超时默认 2 秒返回 Busy。生成模型内容和等待用户均在锁外进行。

受管存储 Read 返回原始 bytes/hash；目录及目标 identity 在内部校验，snapshot 返回 generation。P3 工具读取凭证再绑定 hash + identity + generation。每次写在锁内复核预期 hash：覆盖已有条目或索引都需要 Read 基线，基线不符返回 Conflict；不存在时基线为 absent，create_new 失败不能改为覆盖。不得以 mtime/length 单独代替。Bridge 用 ETag/If-Match，模型保持普通工具 schema，由会话 FileState 传递 hash；自动注入仅在确实送出完整文件时可登记完整读取凭证，截断附件不算完整 Read。

事务序列：安全解析 → 锁与 CAS → 校验新正文及索引 → 同卷暂存 + fsync → 写带完整 before/after 字节及 payload SHA-256 校验的 journal → 替换条目/索引并删除精确旧目标 → fsync 父目录 → 原子更新 catalog hash 与 generation → 移除 journal 标记完成。删除/覆盖前版本保存在受管 trash，P1 保留全部版本、不运行 GC；后续默认保留 30 天，永久删除是显式 UI/CLI 管理操作。删除条目后 broken backlinks 报告待维护，不自动改写另一事实。

多个文件无法仅用 rename 实现整体原子性，故所有**受管读者**同样先拿读锁/检查 journal。P1 提交点为持久化 journal：之前保持旧态，之后按 journal 完整 payload 校验并 roll-forward（即使暂存已丢失亦可重建）；若磁盘已被第三方修改，保留两版报冲突，不覆盖未知写入。取消只在提交点前取消，提交点之后返回实际已提交结果，不能报告取消但后台继续无记录写入。

普通外部编辑器不遵守锁：读取时检测写入中/解析错误并暂时跳过，连续两次稳定读取后接受；同 UID 恶意进程不在锁的防护承诺内。两个正常应用会话同时写同条目必须一个成功、另一个 Conflict；不同条目可短暂串行并正确合并索引。文件被用户重命名可由 stable id 识别，重复 id 则拒绝自动选择。

## 9. SQLite → 文件迁移与回退

### 9.1 映射规则

迁移不调用模型、不根据正文猜所属项目。`dcl memory migrate --preview` 与 `--apply` 为拟新增显式离线管理命令；只有预览/映射、完整备份及验证后才发布。当前远程配置不自动迁移 HTTP 数据，也不因连接失败自动选本地。

| 旧数据 | 文件/归档映射 |
| --- | --- |
| id | 保留为 legacy_id；新 stable id 由固定迁移源 UUID + 原 id 确定性生成，避免不安全 ID 直接当文件名；清单保存一一映射。 |
| content | 原字符串字节保留；活动文件正文只做可逆包装，不 trim/摘要。含对话转录/不合新规范的条目先放隔离 Markdown 及原始行归档，不能为了“事实格式”删内容。 |
| importance | 保存在 metadata.legacy.importance，不映射成四类、不作删除阈值；旧 DTO 返回原值，新文件兼容 DTO 默认 0.5。 |
| created_at | 原值保留，合法时写标准字段；迁移时间单独记录，不能伪装成事实发生时间。非法日期原样归档并标记待修复。 |
| metadata.type | 已精确属于四类的保留；manual 不等于 user，conversation/context_compression/dream 或未知默认 reference + needs_review=true，保留 legacy.type。缺少 Why/How 不编造，转待审状态。 |
| metadata / legacy_source / 未知字段 | 原字符串/JSON、None 与空对象区别均存原始行清单；可解析字段以命名空间保留。metadata 非法 JSON 也不能沿用 local.search 的默认 None 路径丢掉。 |
| similarity_score | 是查询派生值，不用作文件事实；已有外部 archive 中的值保留在源 archive。 |
| settings / dream_sources / dream_runs / migrations | 完整备份，转换为状态清单/运行历史引用；不把它们都变成可召回事实。旧 dream 与原来源均保留，不自动去重删除。 |

归属优先级：用户审阅后的显式映射 > 元数据中可验证的现存 project_id > conversation_id 对应的持久 project/trust + workspace 绑定。多处冲突、会话已删、project_id 不存在、仅在正文提到路径、路径已移动但没有可信映射，均归 `legacy-unassigned`。当下 cwd 绝不成为旧库默认归属。未归属目录默认不注入任何项目，可在管理 UI 选择迁入；这是“保留但不乱召回”，不是丢弃。

所有记录首先实现一对一可核对导出。已知 conversation/raw transcript、疑似凭据、非法 frontmatter、超过限制的记录进入 quarantine；其原文仍保留于私有文件/备份，不展示给模型。只有合法事实条目或用户确认整理后的条目成为活动记忆。预览分别列 total / active / unassigned / quarantined / conflicts，而不是只报告 imported 数。

### 9.2 发布与幂等协议

1. preview 用 SQLite 只读连接和一致性快照；不调用会建表的 `LocalMemory::open`。收集所有表结构和原始行（含 metadata 原文），检查数据库完整性、源指纹和项目映射。无源文件时返回 nothing-to-migrate，不创建空库。
2. apply 进入离线维护：停止 dcl/dcd、dream、Magic Docs 等所有写者，持有全局迁移锁和数据库写屏障；旧版客户端不懂新锁，因此混跑不受支持，必须在切换前确认旧进程退出。不能承诺一个标记能阻止所有旧二进制。
3. 使用 SQLite backup API 或 VACUUM INTO 产生一致备份，包含 WAL 已提交状态；禁止只 copy 主 sqlite 文件。备份完成后按其稳定 snapshot 生成 raw rows、标准 v1 archive（可表示部分）、schema/dream/config 和校验清单。`transfer.rs` 的 parse/冲突经验可复用，但导出本身不足以证明无损。
4. 在同卷 staging 逐条生成确定路径，生成每项目 MEMORY.md、状态与 source→destination 映射；校验原始记录数/ID 集、每条原文字节 hash、元数据原值、归档数、索引与引用。`source_count=active+unassigned+quarantined`，每条只落一个主分类；额外备份副本不计入。
5. fsync 全部资产，写 migration manifest（source UUID、snapshot hash、format version、映射摘要、逐文件 hash、phase）。先发布完整目录，再最后原子更新 `config.json` 的 active backend/files generation。任何阶段失败都保留 SQLite 为旧权威源，不能发布半个项目目录。
6. 相同 source UUID + 原 id + 原始行 hash 重跑时校验现存目标后 skip，**保留迁移后用户编辑**；通过清单区分“已迁移后改动”和“迁移未完成的损坏”。同 ID 源内容变化默认 Conflict，必须显式增量迁移并先 diff。不能用变化的整库 hash 当全新源 UUID，否则会重复全部记录。
7. 崩溃恢复依据 manifest phase：已发布就验证完成，不重复导入；未发布则继续 staging 或恢复旧模式。失败目标保留诊断/备份，不覆盖用户新文件。迁移只切本地后端，不自动开远程、上传内容或改变项目 trust。

迁移只保留已存在数据，不承诺恢复旧 ingest 因 content UNIQUE 曾经覆盖掉的历史 metadata。

### 9.3 回退

无迁移后修改时，显式切回冻结的原 SQLite snapshot 即可；文件目录和清单保留。迁移后已有修改时，先导出完整 v2 文件 archive、暂停所有写者，再生成**新的** legacy SQLite：映射 stable id、原内容、importance 与完整文件 metadata/文件原文 envelope，应用删除 tombstone，不写原备份。验证全部活动条目可表示后才切换。

旧库的 content UNIQUE 无法表示不同 ID 的同正文文件。遇到此类冲突或格式无法无损投影时，禁止声称完整回退成功：返回逐项冲突，保持 files 活动；用户也可显式选择“恢复迁移时旧快照”，但 UI 必须说明新文件仍归档、不会进入旧运行时召回。禁止为通过回退静默合并、加空白改正文或丢 metadata。

不采用双写：无法给普通文件编辑与旧 SQLite 提供一致跨存储事务。files 故障时可只读/无记忆降级并报告；不要自动读取旧全局库混进当前项目。旧二进制直接降级不是安全回退流程，需要先做兼容导出与停机切换。

## 10. 远程 MemEx、dream 与 API/UI

更新：远程 MemEx HTTP 后端与 memory_search / memory_ingest 工具已移除；记忆只保留 files（默认）与 disabled 两种模式，本地 SQLite 仅用于迁移、导出和导入。以下 10.1 是历史设计记录。

### 10.1 远程后端（推荐 D4）

首版保留 opt-in MemEx HTTP，模式明确为 files / memex / sqlite-legacy（回退用）；files 默认。只有 enabled 且明确配置有效 HTTP(S) 后端、模式为 memex 时，内置 memory_search / memory_ingest 才进入运行时目录；仍按原 on-demand 默认，不强制常驻。files/disabled/legacy 模式从 schema、ToolSearch 目录、恢复加载集和执行器全部移除/拒绝。设置页面可保留其配置，但显示“仅远程后端可用”，不能凭 customization override 复活本地工具。

远程没有项目参数，不能宣称自动项目隔离：只在项目显式 opt-in、确认 endpoint 的远端范围后自动召回，或每项目单独 endpoint；首版不默认给全部项目发用户消息。远程工具名/描述明确“远程记忆”；写入沿用远程权限与 Plan 禁写，不能继承本地目录免确认。为所有远程召回设共同 deadline（建议 800ms），不再串行重复六次；失败不写本地兜底。秘密配置走既有凭据服务，不写在 Markdown、URL query 或请求日志。

长期可提供 MCP memory server 替代内置 HTTP 工具，减少专用协议维护；但 MCP-only 一般只提供模型主动工具调用，不能自动满足本需求的引擎召回。若选 MCP，需额外 host-side read-only search adapter、超时/取消/项目 scope/资源信任契约，并关闭另一套自动召回避免重复。此轮不要求立即删远程实现。

### 10.2 dream / AutoDream（推荐 D5）

停止本地自动对话摘录，也停止文件模式的旧摘要追加循环。首期保留 dream 命令/状态入口，files 模式返回明确“不支持旧 SQLite dream，原数据与配置已保留”，AutoDream 标为 paused-for-files，不能返回成功而没有动作；legacy 模式仍可按旧实现使用。

后续 files dream 改为显式整理：读取当前项目快照，生成查重/合并/过时删除/索引精简的 patch 预览，保留各条事实类型及 Why/How，给出来源 id/hash。应用使用同一锁/CAS/journal，源变化拒绝，取消不晚写。不要用 type=dream 破坏四类枚举，也不把模型猜测变为事实。先支持人工 apply；自动 apply 待独立质量/取消/回退测试后才可 opt-in，不因旧 AutoDream.enabled 自动开启。

Magic Docs 按项目 provider 查记忆；未绑定 scope 时返回明确配置错误，不能退回全局。旧 orchestration 的可选 shared_memory 接口若仍保留，需显式传 scope 并停用 raw task transcript 摄取。SDK agent_memory 先保持独立兼容，给出来源标签；项目文件记忆继承父 scope，子代理写权限不强于父，fork/恢复需相同绑定且预算不双算。

### 10.3 CLI / Desktop 接口兼容（推荐 D6）

保留 `/api/memory/*` 路径和原有 JSON 顶层形状作为兼容适配，后端换共享服务；新增 scope/version 能力，不继续以全局库为默认。具体契约：

- 新客户端每次请求携带 conversation_id 或 project_id（二者同时出现必须一致），host 解析 scope；不得让浏览器直接提交任意文件路径。未带 scope 的旧读取默认只看 legacy-unassigned 的兼容视图，旧写入返回 409 `memory_scope_required`；不依赖易竞态的全局“当前选中项目”。这是明确的语义升级，不声称旧客户端零改动。
- v1 `MemoryItem` 的 content 映射正文、id 保持 stable id、importance 使用 legacy 或 0.5，metadata 保持字符串 map（新字段可编码）；新增 v2 item/文档详情接口返回 name/description/type、相对路径、hash/version、诊断。v2 导出含原始 Markdown、scope manifest 和引用；v1 仅作 legacy 投影，不能当 files 备份。
- v1 ingest 的旧 payload 在明确 scope 下仍可用：生成标题/description 的确定性短摘录，type=reference、manual origin；不调用 LLM。新的 UI/CLI 要求填写四类和描述。旧 update 在 scope 中按 ID 定位，保留文件 frontmatter 扩展，不能按“前 100 条”查找。
- 写入需 If-Match/version；兼容调用缺少版本时拒绝已有条目覆盖（428）并给读回路径，不能用 last-write-wins。404 不存在、409 scope/conflict/Busy、422 内容校验、503 存储不可用；错误不回显秘密正文。
- config 分 user-level 后端与 project overrides；旧 auto_ingest 不默默映射为“自动存对话”，files 模式弃用并展示说明。search_top_k 继续生效且受 token 预算上限；importance threshold/compression threshold 的 legacy 设置保留但 files 不使用。
- clear/import/export 必须显式 scope，批量 clear 在 UI/CLI 显示项目及条数并保留用户确认；模型的精确单条维护不触发整库 clear。跨项目 archive 导入先 preview 映射，不以 archive 自报路径取得授权。

`dcl memory` 默认从启动 workspace 解析 scope，可新增 `--project`、管理用 `--unassigned`、`doctor`、`reindex`、`migrate`；命令 list/status 继续 JSON，扩展分页覆盖 >100 条。`/memory entries` 传入 Session scope，展示 name/type/description、打开文件、冲突提示及索引预算；保留 user/project/local/show 说明文件功能和 `# note` 行为。

MemoryPanel 增加项目选择和“未归属旧记忆”视图、name/description/type/Markdown 编辑、索引健康与并发冲突；移除默认 importance slider 和“自动保存完成对话”承诺。新建/编辑/删除仍经服务事务，用户手工编辑与模型编辑共享格式。Desktop 无项目 Home 显示“当前会话记忆”，不冒充跨项目用户池。CLI/Desktop 共用同 data_dir + 同 scope 时必须看到同文件内容，浏览器/手机通过 Bridge 管理服务器文件。

Trajectory 不改成直接读当前文件：历史必须反映当时发送内容。拟在 request snapshot 中增加可选 `memoryContext`（scope/hash、索引/正文来源、实际注入文本或引用既有去重快照块、token 计数、truncation/recall receipt）；正确捕获 user-attached recall。显示读取/保存的普通工具动作，历史文件被删后仍能回放当时信息；v1 快照缺新字段正常渲染。持久化记忆内容会留在会话历史，删除记忆不等于删除历史，界面应明确这一点。

## 11. 分阶段实施与验收

P1 为第 2 轮当前实现；其实际测试/日志以 `codex-round-02-report.md` 为准，P2–P5 尚未实施。每阶段先在任务记录登记需求/命令/证据，再实施；新增用例名称只是建议。对改 core/CLI 的阶段，按 cli-regression-tests 的必跑全量及 G/H、权限 E2、压缩 L 等选场景；Desktop 按 A/B/D 和相关轨迹/服务器项验收，不能用组件测试替代真实 WebView2。

| 阶段 | 范围 | 验收场景与测试入口 | Desktop 影响 |
| --- | --- | --- | --- |
| P0（第 1 轮） | 现状、设计、决策、证据；无实现 | 文档覆盖、源码引用、hash、只新增设计文档、diff 检查 | 无运行行为变化 |
| P1 | scope、格式、FileMemoryStore、锁/CAS/journal、索引维护；独立 `memory::files` 模块、无运行时入口，默认仍旧库 | core `--lib memory::files::tests::`：中英文格式、同仓库 worktree、不同 clone、多根/无项目、坏 YAML/links、长索引、双进程冲突、每个提交断点恢复；所有用例隔离 data_dir | 增加内部服务，旧 UI 尚不切换 |
| P2 | 迁移/导出 v2/回退与只读预览、文件管理 API/CLI | 拟新增 `--test memory_files_migration`：空库/禁用/远程拒绝、坏 metadata、原文字节、含 WAL、未归属、全表 >100 条、重跑/源变化/中断、迁移后修改和删除、逆向 content UNIQUE 冲突；重跑现有 `--test memory_transfer` | Bridge 路径适配与 capability；保留 legacy 操作，不抢先开启新后端 |
| P3 | 首次索引、自动正文召回、停 raw ingest、动态提示词/工具门禁、安全目录写能力 | 拟新增 `--test memory_files_context`、`--test memory_files_permissions`：双协议实际请求、无命中收据、恢复/fork/compact、截断、项目切换、hook wake、禁用、远程失效、ToolSearch/恢复/直接调用绕过；Plan/Router/Coder/ToolScript、symlink/junction/hardlink/TOCTOU/混合 patch | 两端共用引擎；请求快照加入 memoryContext；不可只测 CLI |
| P4 | MemoryPanel、CLI picker、v2 API、Trajectory、Magic Docs；可控用户切换 | `npm run test:run -- MemoryPanel memory trajectory` 后核对实际文件/测试数；CLI peripheral/真实 PTY；Desktop A/B/D、项目切换及服务器/手机 scope、轨迹历史恢复；删除取消/失败、If-Match 冲突、导入预览 | 完整 UI 改版；真 Desktop 与浏览器层分别记录证据 |
| P5 | files dream 整理预览、迁移收尾、跨平台和性能达标后新安装默认 files | core dream、CLI peripheral、服务取消/租约；1k/10k 分层性能和检索语料指标；Linux/macOS/Windows 文件原语验收；旧版本回退手册与显式升级入口 | 文件 dream 入口、旧设置暂停说明；自动 apply 独立 opt-in |

阶段必跑基线命令（实现阶段，先按回归文档加载构建环境并清除宿主测试污染变量，Cargo 串行）：

```bash
git diff --check
cargo test --locked --no-fail-fast -p deepseek-code-core --lib --tests
cargo test --locked --no-fail-fast -p deepseek-code-cli
cargo check --locked -p deepseek-code-desktop --all-targets
npx tsc --noEmit
npm run test:run
```

重点现有入口：core `--test memory_transfer --test dream`、CLI `--test peripheral`，core lib `native_engine::code_prompt_tests` / `tool_loading` / `permissions` / `tools::path_scope`，以及 CLI ToolScript/Super 门禁场景。运行后必须核对实际 test names/counts/ignored，不以过滤命令 exit=0 认定覆盖；新能力必须有自己的断言，不把旧套件通过称作新模型通过。

发布最低门槛：隔离/路径越界/Plan/并发/迁移数据核对全部通过；双协议真实请求证明索引启动加载和自动召回；CLI 与真实 Desktop 共用 scope 的 CRUD 通过；平台未验证则 files 写入保持实验性或只读。性能目标和新 dream 不提前借用旧 SQLite 测试结论。

## 12. 用户已确认的决策（第 2 轮）

| ID | 选项 | 建议与取舍 |
| --- | --- | --- |
| D1 | 仓库及其 worktree 共用；每工作目录隔离；直接用 Desktop project UUID | 推荐 canonical Git common-dir / 非 Git 根 / 多根组合身份。更接近参考行为且两端能一致；若要 worktree 隔离须提前改变 key 算法。无项目默认 session-private，可另选显式全局 user scope。 |
| D2 | Plan 只读记忆；Plan 特许当前记忆目录写入 | 推荐只读，保留本产品当前 Plan 写边界。若选择特许，必须明确包括哪些角色门禁，不得用泛化 own-state 自动放开。 |
| D3 | 普通 ApplyPatch 精确删除；扩展 Write 删除参数 | 推荐现有 ApplyPatch，Read/Write/Edit 负责新增修改；两者都无需专用 memory 工具。当前三个工具本身无法 unlink。 |
| D4 | 暂留显式 HTTP 后端；直接迁 MCP-only | 推荐保留 HTTP opt-in 并隐藏本地专用工具。MCP-only 需额外 host recall adapter 才满足自动召回，不是改个配置就等价。 |
| D5 | files dream 首期暂停，后做预览/apply；首期同时支持自动整理 | 推荐分期预览/apply，减少迁移、并发和事实质量风险。旧 dream 源/摘要/设置全部保留。 |
| D6 | 保留 URL 并升级 scope/ETag；完全新建 v2 路径 | 推荐保留兼容路径并新增 v2 详情/导出；明确无 scope 的旧写返回冲突，不能保持全局隐式写。 |
| D7 | 未归属旧记录先隔离；默认复制到所有项目或当前项目 | 推荐隔离且提供显式分配；可回看全部原文，不造成跨项目召回。 |

D1–D7 已全部按建议确认。P1 的代码/验证状态见下一节及第 2 轮报告；权限与运行时集成、迁移、性能和跨平台发布验收不提前视为完成。


## 13. P1 实现契约与本轮修订

实现位于 `crates/core/src/memory/files/`，仅从 `memory/mod.rs` 导出独立模块，既有 MemEx、SQLite、dream、工具、CLI 和 Desktop 调用不变。没有新增 Cargo feature 开关：没有调用者本身即为隔离边界，运行时后端开关留给 P2/P3。新增直接依赖 `unicode-normalization`、`unsafe-libyaml` 均复用 lockfile 已有版本，后者先扫描 YAML 事件拒绝 anchor/alias/tag/深嵌套，避免先构造 Value 扩张别名；普通引号字符串中的 `&*!` 正常接受。

- **Scope**：`MemoryScope::for_workspace(roots, trusted, home, private_owner)` 对空根/默认 Home 使用持久 private owner；`project` 从 host 授权根求 Git common-dir/集合身份，`private` 可供 owned child 继承。P1 不查询持久会话数据库、也不写全局 projects.json；host 绑定/项目迁移仍属后续集成。`state/<key>/scope.json` 保存完整身份并在锁内核对。Windows 使用 canonical 路径 UTF-16LE；Unix 原生字节。Home 参数及 trusted 位绝不能来自模型自行声明。
- **格式**：安全文件名不强制含 id；新写入补 UUID/UTC metadata，更新保持已有 id/created_at，保留未知字段。`Why` / `How to apply` 使用代码围栏外的二级 Markdown 标题并有实质内容。NFC + case folding 的特殊映射表来自 Unicode 14，普通小写使用 Rust Unicode 表；未来升级需重新验证重复名。坏文件保留、给诊断、不入索引；重复 name/id 的全部冲突项排除，写入不静默选一条。
- **状态与持久性**：`state/<key>/{lock,scope.json,journal.json,catalog,generation}`，`trash/<key>/<transaction-uuid>/<filename>`；catalog 为活动 filename/hash 集合摘要，使仅改正文也推进 generation。journal 保存 before/after 原文、旧/新 catalog、旧/新 generation 与整个 payload 校验值；它是损坏检测而非对同 UID 的加密认证。索引修复同样备份旧索引并使用事务。
- **路径边界**：调用方先创建并选定绝对 data_dir。Unix 根由 `/` 逐组件 no-follow 打开，再用 dirfd 相对操作；新目录 0700、新文件 0600，拒绝非普通文件与 nlink != 1。Windows pin 从卷到 data_dir 的祖先及受管子目录句柄、不分享 DELETE，拒绝 reparse/junction/多链接；替换使用 MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)。Windows 文件 flush 与 POSIX 父目录 fsync 的断电保证不同，本轮只用进程退出模拟故障，未声称通过断电/Windows实机验收。
- **一致性**：受管操作在跨进程项目锁内，CAS 对原始完整字节 SHA-256。读取同句柄两次，比较 bytes/stat 并复验路径 identity，检测普通外部编辑中的变化；外部进程不遵守协议时仍不承诺任意竞争下完整隔离。路径替换永不 truncate 既有外部 inode，最终 rename/unlink 不跟随被替换的链接。锁等待默认 2 秒且可取消；持久 journal 之后不再按取消中止提交。
- **恢复边界**：测试断点为 staged / journal / entry / index / catalog / generation / cleared。持久 journal 后重开自动 roll-forward；若任一路径既非 before 又非 after，保留 journal/外部版本并报 RecoveryConflict。提交点前进程退出可能遗留不发布的 `stage-*`，不参与 `.md` 扫描；P1 不自动清除来源不明暂存，后续 doctor/GC 负责核验后清理，不能按通配符误删用户文件。
- **索引和上限**：磁盘完整索引上限 8 MiB，活动扫描最多 10,000 个 `.md` / 64 MiB，目录枚举最多 20,000 项，超限明确失败。上下文默认 200 行/25 KiB/4096 tokens，包括省略标记；可传真实 tokenizer，默认 byte 数保守计数。极小/零预算容不下标记时返回空文本并以 `omitted/truncated/marker_included` 明确报告，P3 用预算外状态解释未加载。80% 提示、模型工具保存结果 DTO 和实际会话注入属 P3/P4。
- **只读/权限衔接**：P1 `open/read/snapshot` 可能恢复未完成事务，snapshot 还会修复索引。这些是存储维护 API，不是已实现的 Plan 工具授权。P3 必须在受管维护完成后获取稳定只读快照，Plan 模型不得调用任何 mutation/repair；若底层目录只读且需恢复，返回不可用，不绕过 OS 限制。本轮未添加任何目录免确认能力。

本轮明确未做：迁移/回退工具、召回排序与性能、prompt/工具暴露切换、Plan/角色/sandbox adapter、Bridge scope/ETag、CLI/Desktop 管理界面、files dream。旧 SQLite dream 保持现状；D5 的暂停在启用 files 后端时生效，不能提前改变旧路径。

## 14. 第 3 轮 P2 接入修订（2026-11-13）

用户本轮将 P2 明确扩大为引擎/工具/提示词和两端调用链接入，且允许旧库迁移**或兼容**。因此本轮先上线文件服务和显式旧库兼容，不把原 §11 的离线迁移作为前置；原 P3/P4 的相关接入在本轮完成，批量迁移、原生界面验收及性能优化仍单独验收。本节是本轮实现契约；实际通过/失败数量以 `codex-round-03-report.md` 为准，不以本文描述代替测试结果。

- `services::memory_files::FileMemory` 从持久可信 project/session 选择 scope。引擎不接收模型自报的项目键；会话首次绑定的 scope 保存在 `peripheral_settings`，恢复时重新验证可信目录与身份。显式宿主 workspace move/add-dir 事务撤销旧绑定，下一轮重算；本轮持有的旧能力检测到变化即拒绝。Read 回执按 session + scope + filename 保存内容 hash，partial Read/召回片段不授予写回执，owned child 重新绑定自己的会话。
- 文件模式为启用且 `backend_url=local`；保持现有后端设置结构。`search_top_k` 生效（`auto_search` 已随主线的自动记忆一并移除；文件模式在记忆启用时按当前 query 召回）。`auto_ingest`、importance/compression 设置仅留给 legacy/remote。文件模式不再自动保存完整对话。索引每次实际请求按当前快照重建，包含 scope、generation、hash；两种协议的既有 request snapshot 记录实际 system prompt 和 user recall，因此 Trajectory 不直接读取当前记忆文件，也暂不新增专用 `memoryContext` 数据结构。
- 召回复用 Latin/CJK tokenizer，首版对受管快照线性评分 `0.25 name + 0.50 description + 0.25 body`，阈值 0.15，同分按 filename；最多 5 条，空余容量扩展最多 2 条同 scope 的一跳链接，按规范化 name 去重。**尚不实现倒排缓存、上一轮 query 加权或 embedding**。索引/正文用 UTF-8 byte 保守估算 token；200 行/25 KiB/4096 tokens 的硬上限不变，总额受 8192、窗口 10% 和估算输入余量约束，索引优先；wrapper、路径和元数据计入预算。整行截断，标注省略数；召回 XML 字符转义。命中的正文沿用 turn_context；hit/zero-hit 在 peripheral_settings 保存 session + scope + 用户消息 hash/occurrence 的结构化收据，无命中不改写用户消息，尚未引入独立 receipt 表。
- 普通 Read/Write/Edit 和只含一条 Delete File 的 ApplyPatch 路由到 FileMemoryStore。只允许当前受管目录的一层合法文件名，拒绝其他 scope/state/trash、`..`、混合 patch、通过其他路径别名取得免确认。存储层继续 no-follow、锁、CAS、journal。ApplyPatch 删除也检查 Write/Edit 路径规则；显式 ask/deny、工具禁用、角色、hook、Plan 均保留，shell/MCP 不得继承此例外。这里的模式/角色约束属于模型调用链；用户直接在管理命令或面板编辑是独立的显式操作，不把该操作权限传给模型。Plan 使用只读打开：不创建目录、不修复磁盘索引、不恢复 journal；索引只在内存规范化。待恢复 journal 要求可写维护操作并保留原件。
- `memory_search`/`memory_ingest` 的原始定义保留以便设置界面和远程使用，但本地/禁用时从模型 catalog、ToolSearch 与直接执行门禁排除。仅 enabled HTTP(S) 后端可加载，仍遵守 customization 的常驻/按需/关闭规则。远程故障不回退到 SQLite 或文件。首期继续沿用用户显式配置的全局 HTTP endpoint 和现有超时；未实现 §10.1 建议的项目级 endpoint/opt-in 与统一 800ms deadline，不能宣称远程具有文件模式的项目隔离。项目级远程配置留给后续独立变更。既有协议测试改用本地 HTTP MemEx mock，保持原有断言。
- **兼容而非迁移**：原 `memory.sqlite3` 及 dream/caveman/FTS 等数据原位保留；它整体是无可靠归属的 legacy archive，不自动注入项目。`dcl memory files [--session ID | --project ID] list|put|delete` 操作文件，无选择时使用明确启动 workspace（Home 要求 session）。原 flat `dcl memory list/add/export/import/...` 暂保留为旧库兼容命令，help 明示；`/memory entries` 改为会话文件面板，`/memory legacy` 保留旧面板。终端编辑通过临时 Markdown 草稿和编辑器完成，成功后经 CAS 提交。旧数据的实际转换、超限隔离与 v2 archive 待后续阶段实施。
- Desktop 默认 FileMemoryPanel 使用当前会话，未开会话时用当前选中可信项目。显示 name/type/description、Markdown、索引省略数和坏文件诊断；取消删除、冲突保留草稿、scope 切换后不接收旧请求结果。显式 legacy archive/remote settings 入口保留旧管理页。`/api/memory/search|ingest|items/{id}` 使用 scope 和内容 hash version，文件响应标记 schema_version=2；更新/删除缺少版本拒绝，旧无 scope 写入拒绝，Conflict 为 409。`stats/export` 要 scope；批量 `clear/import` 暂明确拒绝并给出版本化单条/legacy 路径。旧接口完整移至 `/api/memory/legacy/*`；不允许旧客户端无 scope 写到文件模式。
- 文件服务首次使用记录 `memory.files.active=true`，dream 服务从此暂停（生成提交前再检查），原数据/配置保留。**只有从未使用文件服务的纯 legacy 数据目录仍保留旧 dream 行为**，用于兼容既有工作流；不把这个兼容行为称为文件 dream。回退旧程序可以继续读取原 SQLite，新增文件保持原样但旧程序不可见；本轮没有自动反向同步或双写，回退不等于把新事实转回 SQLite。

后续阶段调整：P3 完成显式迁移/归属预览、v2 archive 与逆向导出；P4 完成真实 Desktop/WebView2、终端 picker、Windows 文件原语及规模/延迟验收，按结果优化阻塞读取/缓存与更多诊断；P5 再设计文件 dream 的预览和显式应用。不得把本轮 Linux 单元/本地 mock 的结果等同于其他平台或原生 UI 验收。
