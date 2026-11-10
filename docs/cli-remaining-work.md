# deepseek-code-cli 剩余工作清单（新 session 入口）

维护说明：长期开发记录、验收证据和脚本已迁到相邻的 `dev-support/deepseek-code`，可复用环境放在 `dev-support/environments`。下文旧 `temp/claude-code-cli-dev` 引用对应迁移后的同名文件；新记录不再写入临时目录。

更新：2026-10-18。本文汇总独立 Rust CLI 与原版 Claude Code 对齐的未完成项，供新 session 直接接手。详细审计依据见 `D:/code/project/project-harven/temp/claude-code-cli-dev/remaining-parity-acceptance-checklist.md`（以下简称"清单"）。

## 1. 起步

- 开发目录：`D:/code/project/project-harven/deepseek-code-desktop`，分支 `feat/claude-code-optimization`。2026-10-17 已合并全部 worktree 和功能分支并删除，直接在此开发，不再使用 worktree。
- 未 push。合并备份与已删除分支的 SHA 在 `D:/code/project/project-harven/temp/merge-backup/`；`git stash@{0}` 为合并前原 checkout 的未提交改动（已被合并内容取代，保留以备核对）。
- 开发记录目录：`D:/code/project/project-harven/temp/claude-code-cli-dev`（状态见其中 `handoff-current.md`、`README.md`）。

## 2. 约束

- `claude-code-cli-doc` 是 CLI 的**用户使用手册**，`deepseek-code-docs` 是 Desktop 的用户文档；开发记录、终端证据、测试脚本一律写到 `temp/claude-code-cli-dev`。
- Cargo 命令串行执行，统一 `CARGO_TARGET_DIR=D:/code/project/project-harven/deepseek-code-desktop/target`。
- 节奏：开发一个、验证一个、commit 一个。英文提交信息，沿用 `feat:`/`fix:`/`docs:` 前缀，结尾 `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`。不 push，不自动关机。
- 只读参考：`D:/code/project/project-harven/claude_code_src`（下文 `reference:`）、`D:/code/project/project-harven/codex`、需求列表 `D:/code/project/project-harven/instructions.txt`。官方程序 `E:/package/claude-code/2.1.295/claude.exe`。
- 源文件多为 CRLF：用 Python 脚本"读入→统一 LF→精确替换→写回 CRLF"编辑（示例 `temp/claude-code-cli-dev/logs/fmt-backup/relay_cli.py`，其中路径需改成当前目录）；`rustfmt` 只用于新文件或测试文件，避免无关格式改动。提交前 `git diff --check`。
- 结束测试进程按进程名（`dcl.exe`、`remote_control_mock.exe`、`node.exe`）过滤；按 CommandLine 过滤会误杀当前 bash。
- PowerShell 文本显式 UTF-8，避免控制台表格。不提交凭据；Remote Control 只用自身 token（`CLAUDE_BRIDGE_OAUTH_TOKEN`），绝不发送 Anthropic OAuth 凭据。
- 保留 `D:/code/project/project-github/qwerty-learner-cli-pass1`、`-cli-pass2` 的未提交改动。
- 浏览器测试用 `D:/code/project/project-browser-use/deepseek-browser-use`。

## 3. 验证工具

- 测试（最近结果）：
  - `cargo test --locked -p deepseek-code-core --lib --tests`：543 通过 / 0 失败 / 4 忽略
  - `cargo test --locked -p deepseek-code-cli`：251 / 0 / 1
  - `cargo check --locked -p deepseek-code-desktop --all-targets`
  - `cargo build --locked -p deepseek-code-cli`
- 真实终端（Windows ConPTY），目录 `D:/code/project/project-harven/temp/claude-code-cli-dev/`（旧 spec 中 `evidenceDir` 仍指向 `claude-code-cli-doc`，复用前修改）：
  - `node terminal-harness.mjs <spec.json>`：启动 CLI；运行中向 `<evidence>/input.jsonl` 追加 `{"write": "..."}` 或 `{"snapshot": "name"}` 行进行交互。
  - `node logs/fmt-backup/screen.js <abs path .screen.json> N`：读屏幕快照。
  - `integration-acceptance-fixture.mjs <prefix>`：本地模型 fixture（STEP_EDIT / STEP_WRITE2 / STEP_SLOW / STEP_TOOLS 等脚本化步骤）。
  - `crates/core/examples/remote_control_mock.rs`：Remote Control 模拟服务（`cargo build --locked -p deepseek-code-core --example remote_control_mock` 后运行 `remote_control_mock.exe 5516 rc-mock-token`）；`node logs/fmt-backup/rc_answer.mjs allow|deny|interrupt|state` 扮演远程客户端。
- 测试用临时目录与本地 mock 端点，不用真实凭据。

## 4. 已完成（不要重做）

- 第一阶段集成：MCP 输出替换 `ef22c86`、启动工具规则 `94718dc`、主题修复 `8be287b`。
- 已配置 MCP 工具命名空间 `mcp__<server>__<tool>`：`be8132e`。
- command hook `if` 条件过滤：`7d3c9fe`。
- `/plan <描述>` 进入计划模式并提交任务一次：`08e92cf`。
- `/remote-control`：自定义服务端地址、mock、消息/控制转发、重连、拆除 `f2748fa`；工具审批转发与远程中断 `a01e83a`（ConPTY session 4 已验收并记录）。
- `/remote-control listen` 与 `dcl remote-control connect`：dcl 之间直连的终端镜像。会话以 `--resume` 在 pty 中的子 dcl 继续，本地终端与一个远程客户端共用同一界面（全部命令、菜单、快捷键）；Noise NNpsk0 + 一次性 token；重连时用 vt100 快照回放滚动历史与屏幕；`Ctrl+]` 断开、新连接替换旧连接；子 dcl 经本机控制端口执行 `listen/status/disconnect` 并接收连接提示。Linux 本机双 pty 验收（`deepseek-code-dev/runs/p2p/duo.py`）；跨机器、macOS 与 Windows（ConPTY、按键事件转字节）未验证。
- 清单"Already implemented"一节：SDK hook 回调、SDK MCP 传输、SDK 自定义子 agent、`/statusline setup`、LSP 生命周期、目标提议、owned-child 恢复与后台任务、内置资源工作流等。

## 5. 待开发（按优先级）

每项完成标准：真实触发 + 精确 payload + 事件语义 + 超时/取消 + 会话隔离 + 恢复时不重复；有单元/集成测试，并在需要时做 ConPTY 验收。schema 条目或 `/hooks` 列表不算完成。

### P1 Hook 类型与选项（清单 H2）

入口：`crates/core/src/services/tool_hooks.rs`（`CommandConfig` 约 174 行；loader 只接受 `type: command` 并拒绝 `async: true`；`ActiveHook::condition_matches` 约 77 行，新类型必须复用）。参考：`reference:src/schemas/hooks.ts`、`reference:src/utils/hooks.ts`、`reference:src/utils/hooks/exec{Prompt,Http,Agent}Hook.ts`。

1. [x] `once`：首次成功后按会话持久化注销；并发、重载、失败/取消重试已验证（`5b026c8`，详见开发记录 `once-acceptance-20261018.md`）。
2. [x] `statusMessage`：CLI/worker 运行进度、完成/失败/取消清除及 ConPTY 恢复验收完成（`a2a0366`，详见 `status-message-acceptance-20261018.md`）。
3. [x] HTTP hook：本地 HTTP fixture、显式 header 环境变量白名单、超时/错误/取消、权限组合与跨进程 once 恢复已验收（详见开发记录 `http-hook-acceptance-20261018.md`）。
4. [x] Command `async`：会话托管的后台执行、跨进程一次性上下文投递、取消/超时/关闭及 Windows 进程树清理；CLI 集成与 ConPTY 验收完成（详见 `command-async-hook-acceptance-20261018.md`）。
5. [x] Command `asyncRewake`：隐含后台执行、exit 2 阻塞反馈、CLI 空闲/运行中恰好一次投递及取消/关闭/恢复隔离；CLI 集成与 ConPTY 验收完成（详见开发记录 `async-rewake-hook-acceptance-20261018.md`）。
6. [x] Prompt hook：有界单次无工具 provider 评估、事件限制、输出校验、失败/取消、once 恢复与权限组合；OpenAI/Anthropic mock 及 CLI Write 阻止集成验收完成（详见开发记录 `prompt-hook-acceptance-20261018.md`；含 built-in provider，传输重试受 hook timeout 约束）。
7. [x] Agent hook：有界多轮验证、独立只读权限上下文、结构化结果、取消/会话关闭清理及 once/组合优先级；双协议 mock 与独立 CLI 集成验收完成（详见开发记录 `agent-hook-acceptance-20261018.md`）。

未支持的选项要明确报错，不能静默接受。验证命令 hook 与 SDK 回调组合时的顺序和 deny/ask 优先级。

### P2 Hook 生命周期事件（清单 H1）

入口：`tool_hooks.rs` 的 `HookEvent`（当前 24 个）；loader 会跳过未知事件名。参考：`reference:src/entrypoints/sdk/coreTypes.ts`（27 个）。

- [x] 第一批生命周期事件：`Notification`、`StopFailure`、`Setup`、`ConfigChange`、`InstructionsLoaded`、`CwdChanged`。已接入独立 CLI 实际路径、精确 payload/matcher、输出语义、SDK 回调与类型限制；once/if/status/取消/超时复用并测试。core 595/0/7、CLI 261/0/1；transcript 仅在匹配 hook/SDK 实际执行前按 run 生成一次，失败警告不阻断分发；逐事件差异与不可触发子场景见开发记录 `lifecycle-events-acceptance-20261018.md`（ConfigChange 使用轮询/使用前检查，拒绝后阻止后续使用；不声称与原版 watcher 完全等价）。
- [x] `FileChanged`：已接入会话级 watchPaths 监视、basename matcher、精确 payload 与通知型输出；通知通过 tracing，独立 CLI 集成确认 debug-file 收到消息且 stderr 不被直接写入；本次 Windows core 613/0/7、CLI 268/0/1（通过/失败/忽略），详见 `watchpaths-filechanged-acceptance-20261018.md`。task10 已修复跨平台编译及 macOS 系统路径别名；Linux/macOS 完整 core/CLI 与真实模型冒烟已通过，见开发记录 `unix-build-acceptance-20261018.md`。
- [x] `Elicitation`、`ElicitationResult`（form 模式最小可用链路）：stdio / SDK MCP 反向请求、服务器 matcher、hook 直接 accept/decline/cancel、结果修改/阻止、requested_schema 校验、CLI/worker 交互和 SDK 控制应答；无交互默认 cancel。URL 模式及异步 elicitation hook 明确报错，不宣称支持。验收详见开发记录 `elicitation-hooks-acceptance-20261018.md`。
- [x] 团队事件 `TaskCreated`、`TaskCompleted`（完成前可拒绝）、`TeammateIdle`（阻止 idle 并带反馈继续工作）：payload/无 matcher 语义按原版，TaskCreated 阻止时回滚任务；TeammateIdle 阻止时恢复 teammate（最多 3 次）。覆盖见 `team-t2-acceptance-20261020.md`。

### P3 Hook 输出与环境（清单 H3）

入口：`crates/core/src/services/hook_callbacks.rs`（仍拒绝 async SDK 结果；watchPaths 已支持）。设计参考 `temp/claude-code-cli-dev/sdk-hook-output-parity-plan.md`。

- [x] `SessionStart.initialUserMessage`：command/HTTP/SDK 最后非空输出按实际观察顺序胜出；打印与 SDK 入口消费一次并先于调用方输入，持久化 hook 来源且不进入用户授权账本；UserPromptSubmit 正常触发一次。REPL/clear/compact 不自动提交，resume 只消费本次新输出，不重放历史。详见开发记录 `initial-user-message-acceptance-20261018.md`（含 ConPTY 与 auto classifier 隔离验收）。
- [x] `watchPaths`：会话级原生 watcher，忽略初次发现、500 ms 去抖、add/change/unlink；缺失目标监视祖先后转为目标；动态集合替换、cwd 重置与会话隔离。审查修订：空闲不递归扫描，通知/pending 驱动 200 ms 采样，缺失路径 2 s metadata 兜底；spawn_blocking 执行快照与正常关闭；扫描上限 10,000 条，超限告警并降级到已知文件；关闭确认最长 2 s，超时告警。core watcher 8/0/0，含空闲计数、超限降级、关闭超时测试；Windows 全量 core 613/0/7、CLI 268/0/1。详见 `watchpaths-filechanged-acceptance-20261018.md`。历史 Linux inotify 5/0/0、macOS FSEvents 6/0/0 仅对应修订前隔离探针，本次按要求不重跑；task10 已完成完整跨平台编译/测试及 macOS FSEvents 系统别名修复，见 `unix-build-acceptance-20261018.md`；非 Windows 会话环境文件已在 task11 验证（见 `unix-session-env-acceptance-20261018.md`）。
- [x] 会话环境文件（`CLAUDE_ENV_FILE`）：Windows 按原版返回 null；Linux/macOS 在 Setup/SessionStart/CwdChanged/FileChanged 的非 PowerShell command hook 中提供私有 0700/0600 文件，经 Bash/sh/zsh 的 `eval` 前注入，cwd 重置清空 cwdchanged/filechanged，会话结束删除目录；符号链接拒绝、会话目录名 SHA256。Windows core 618/0/7、CLI 268/0/1；Linux/macOS `session_environment` 4/4、`protected_paths` 与 `fs::tests` 通过；两平台真实模型 Read+Bash 冒烟（含环境变量与清理）通过。详见 `unix-session-env-acceptance-20261018.md`。

### P4 团队（清单 T1、T2）

入口：`crates/core/src/tools/coordination.rs`（当前 team 是持久任务记录）、`crates/core/src/native_engine/delegation.rs`（顺序执行委派）、`crates/core/src/tools/agent_messages.rs`（UUID 寻址文本消息）。参考：`reference:src/tools/TeamCreateTool/`、`TeamDeleteTool`、`SendMessageTool/SendMessageTool.ts`；原版受 `isAgentSwarmsEnabled()` 开关控制。

- [x] T1：`TeamCreate`/`TeamDelete` 按原版输入输出；团队文件与 `name@team` 成员持久化在运行时目录；`TeamSpawn`（本项目批量入口）并发运行多个命名 teammate，各自独立会话、transcript 与 fork 权限；leader 取消时成员标记 `cancelled`，重启后 active 但无对应进程的成员标记 `stale`；有活跃成员时拒绝删除并清理目录；`--agent-teams`/环境变量开关。core 625/0/7、CLI 269/0/1，含并发屏障、取消与独立 CLI 集成测试；差异（进程内并发替代 tmux、数字后缀命名、无远端 killswitch）见 `team-t1-acceptance-20261020.md`。
- [x] T2：`SendMessage` 支持 teammate 名字与 `*` 广播（未知名字报错、持久化、空闲成员唤醒）；`shutdown_request`/`shutdown_response`/`request_status` 按 request id 关联，拒绝保持存活、批准标记 stopped、超时变 timeout；teammate ExitPlanMode 路由给 leader（不得自批，批准进入 AcceptEdits、拒绝保留 plan mode）；`/list-agents` 增加 kind（team-lead/teammate/subagent/session）与团队状态。core 632/0/7、CLI 269/0/1；差异（进程内同步成员在下次恢复时消费消息）见 `team-t2-acceptance-20261020.md`。

### P5 SDK 兼容（清单 S1、S2）

- [x] S1：`crates/core/src/services/permission_updates.rs` 支持 session/userSettings/projectSettings/localSettings（cliArg 明确报错），六种操作（add/replace/removeRules、setMode、add/removeDirectories）；混合批次全有或全无，设置文件原子写与跨进程锁、BOM/未知字段保留、未信任项目拒绝写项目设置、deny 优先；`load_settings` 按 user→project→local 读取并由 CLI/SDK 合并，附加目录进入 path scope。core 635/0/7、CLI 270/0/1，含 SDK 控制协议集成测试；defaultMode 仅持久化未参与启动选择等差异见 `sdk-permission-updates-acceptance-20261020.md`。
- [x] S2：`crates/cli/src/sdk_agents.rs`、`sdk_initialization.rs` 实现 AgentDefinition 其余字段——mcpServers 字符串名限制子 agent 工具、skills 预加载、memory 三作用域文件、permissionMode（不得越权、拒绝 auto）、effort（命名等级）、initialPrompt 与 reminder；`background` 与内联 mcpServers 明确报错。新增 `initialize.agent` 主线程选择（本项目扩展），`promptSuggestions` 在成功回合后输出 `prompt_suggestion`，`agentProgressSummaries` 给 `agent_progress` 增加 summary。core 636/0/7、CLI 274/0/1；逐字段表与差异见 `sdk-agent-definition-acceptance-20261020.md`。

- [x] S3：参考 SDK 消息与控制协议。stream-json 输出参考消息（`system/init`、按内容块的 `assistant`、`user` 工具结果、带 `subtype`/`num_turns`/`duration_ms`/`total_cost_usd`/`permission_denials` 的 `result`、`--include-partial-messages` 的 `stream_event`），旧事件行保留；新增控制请求 `mcp_status`、`get_context_usage`、`get_settings`、`apply_flag_settings`、`set_max_thinking_tokens`（映射为 effort 等级）、`cancel_async_message`、`mcp_toggle`、`mcp_reconnect`、`reload_plugins`、`stop_task`、`rewind_files`（按 SDK 消息 uuid 定位检查点，支持 dry run）、`seed_read_state`（编辑不依赖读取缓存，仅校验参数）；查询类、`set_permission_mode`、`cancel_async_message` 可在回合中处理；`mcp_set_servers` 明确报错；接受参考 SDK 的 `session_id: "default"`。提交 `834f3d0`、`1bc2985`、`8192e14`、`4abac75`、`6ddb977`、`30a3db3`、`a869d49`；CLI 314/0。Java SDK（`litongjava/deepseek-code-java-sdk`，Java 8，无依赖）端到端驱动 `dcl`，tio-boot 示例服务验证一次性查询、SSE、会话、回合中状态与中断；记录见开发目录 `runs/sdk/sdk-acceptance-20261024.md`。

### P6 命令内部差异（清单 C1、C2）

- [x] `/plan` allowedPrompts：输入 schema 与校验（Bash、非空、≤500 字符、≤32 条）；批准结果与 teammate 审批请求携带语义提示，但不转成规则、不放宽 Bash（批准后仍需确认）；非法工具在弹审批前拒绝。clear-context 归档/保留 UUID 由既有双协议测试覆盖。详见 `p6-plan-memory-config-acceptance-20261020.md`。
- [x] `/workflows`：本地范围定义为 `<data-dir>/workflows/*.md` 与 `<workspace>/.claude/workflows/*.md`（project 覆盖 user）；支持 list/show/run，run 以文件内容+参数启动后台任务，状态/取消用 `/tasks`；`hosted:` 明确报错。托管 workflow 未实现。详见 `p6-workflows-batch-acceptance-20261020.md`。之后新增脚本工作流：`Workflow` 工具执行 `.claude/workflows/*.js`（QuickJS，`agent()`/`parallel()`/`pipeline()`/`phase()`），无参数的 `/workflows` 浏览、查看和停止本会话的运行（`69ca0e9`、`1fb6f2b`）。
- [x] `/batch`：保留显式 plan→run→publish UX（与上游自动编排的差异已说明）；成功、并发上限、权限继承、取消、路径重叠、worker 失败与 supervisor 心跳过期标记均有测试；`--publish` 之外不推送；真实 PR/远端发布未验证。
- [x] `/memory`：`/memory`/`list` 列出 user/project/local 文件与状态；`user|project|local` 用 `$VISUAL`/`$EDITOR` 打开并在缺失时创建；`edit` 兼容 project，`show` 保持打印项目指令。
- [x] 交互式配置/powerup：既有文本向导覆盖选择/编辑/保存/取消；`/cancel` 与不完整输入路径测试断言配置字节不变；本次无新增交互布局，外观差异属既有实现差异。

### P7 需要真实环境或账号的验收（清单 D、C3）

代码已存在，缺的是真实环境验证；每项开始前向用户确认所需账号/环境。

- 真实 Remote Control 服务与客户端（授权账号）。
- IDE 扩展（真实扩展版本、选区、diff 接受/拒绝、断线重连）。
- Design 服务（授权、上传、渲染；浏览器测试按 deepseek-browser-use 流程）。
- 真实 LSP 服务器（rust-analyzer/TypeScript/Python，九种操作、写入后诊断、重启）。
- [x] Linux x86_64 / macOS arm64 core 与独立 CLI 构建、完整测试、真实 Anthropic 兼容接口 print / stream-json / Read 冒烟。task10 验收：Windows core 615/0/7、CLI 268/0/1；Linux core 613/0/7、CLI 267/0/1；macOS core 615/0/7、CLI 267/0/1（通过/失败/忽略）。Windows CLI/Desktop all-target check 通过；不支持的 Unix 本地截图及 macOS 按键/侧键明确报错；平台 cfg 差异、失败修复和未验证项详见开发记录 `unix-build-acceptance-20261018.md`。
- [x] 与官方 CLI 的终端对比：Linux 下用 Claude Code 2.1.310（隔离配置目录）与 dcl 在同一 DeepSeek 接口、同模型、120x32 下逐场景对比原始 ANSI 与最终屏幕。已修复：审批菜单与编辑 diff 预览（`8ded02b`）、斜杠菜单（`7e1ec0c`）、输入按词换行（`00304bf`）、`❯` 与启动标题（`81c72dc`）、回答按词换行与悬挂缩进（`285edb9`）、工作区内只读 shell 命令免询问（`7ccf445`）、中断提示（`136b133`）、单行页脚（`8a0a446`）。启动到提示符 dcl ≤0.2 s、官方 ≈0.4 s。第二轮已对齐：补全菜单移到输入框上方（`366b2b7`）、连续只读调用合并为一行摘要（`93206c3`）、Bash 审批记住命令前缀（`95c5610`）。第三轮（功能）：回答后清除审批框（`8e8a707`）、审批可附带说明并作为用户原话保存（`c3118d9`）、Tab 补充说明 / 直接 No 停止本轮 / 切换自动模式（`a876fe2`）、先跟选项的命令按程序记住（`ecb1606`）。路径类命令（`touch`、`rm`）的"允许访问目录"未照搬。第四轮：斜杠命令排序（`0f57015`）、运行中输入的消息在下一个工具结果后交给模型（`05a7f8a`）、审批菜单 Esc 等同 No（`1b5c291`）、Ctrl+O 全屏详细记录（`f6548c9`、`9700f69`）、`/resume`、`/rewind` 选择列表（`fa6aa1d`、`d121eab`）、`@` 目录（`774ec8d`）、`?` 三列面板（`7d0988d`）。第五轮：计划审批菜单（`3c3d107`）、`/mcp`（`5cc8155`）、`/hooks`（`b98fc22`）、`/tasks` 与后台命令提示（`21191dd`）、Ctrl+B 与 ctrl+x ctrl+s 把运行中的命令转到后台（`ce886a2`、`a976075`）、`/resume` 预览/重命名/全部项目（`3ba0c52`）。后台命令结束自动开始新一轮（`ae1122a`）。`/resume` 分支与大小（见 git log）。第六轮：输入框快捷键 ctrl+g/s/_/y/z、`\`+Enter、Esc Esc 清空（`36ad04b`），ctrl+t 待办（`297badf`），alt+p 模型与 effort（`a3c6f5c`），设置对话框与 `/context` 方格（`960ea10`），`/permissions`、`/memory`、`/agents`（`5be7fca`），目录信任与 bypass 确认（`da9b700`），`/mcp` 隐藏预设（`fb41a4c`）。第七轮：设置对话框 Stats 页（`c7c87ea`）、`/context` 首次请求前估算与 `/resume` 大小（`bfbd15b`）、后台子代理与 `← for agents`（`6f29c18`、`c319c5b`）。第八轮：运行中输入框快捷键（`253438b`）、后台子代理审批在主界面询问（`d46fa8d`）、`/tasks` 查看子代理记录（`b8f55d1`）、工作流技能（`8bdc9ab`）、`/loop` 与 `/scroll-speed`（`278db2e`）、`/copy` 与 `/export`（`3bbe77d`）、`/compact` 输出（`ec4d004`）、思考预览与 `Thought for`（`b9d2631`）、`[Image #N]` 图片占位符（`ea5200e`）、`/rewind` 行数与确认页（`d2b6643`）；第九轮：图片回显与运行中消息附图（`383cfd8`）、回退后重绘并放回输入框（`84cc16f`）、思考时不显示详情行（`4183dcf`）、回退选项编号与 Summarize（`634e514`）、`/ide` 选择（`c17991f`）、`/terminal-setup`（`8fc90e0`）、Config 的 Editor mode 与原地切换（`54c75d9`）、窗口标题（`aeaece7`）；`/login` 按要求不改。剩余：Windows/macOS 终端未在本机验证（本机只有 Linux 目标）。记录见开发目录 `runs/ux/ux-parity-20261024.md`；Windows ConPTY 下未重跑。
- qwerty-learner issue #1095：先读 `issue1095-pass2-final-review.md` 等已有记录，目标仓库不提交。

### P8 提示词与工具对齐 Claude Code 2.1.302

- [x] 系统提示词改为参考布局：一行身份 + `code.md`/`memory.md` 各带缓存断点，环境与自定义提示词在后（`7afa5a6`）；第一条用户消息附项目说明与会话级 Git 快照（`29d5bf8`、`e72a2d4`）；钩子附加内容、技能正文、记忆召回附在当轮用户消息（`0342909`、`8ab6c96`）；压缩后重新附上已调用技能（`8d5022f`）；首条消息后发送会话级提示（`8b59c78`）。
- [x] 工具名称、参数与行为对齐：Read/Glob/Grep/Bash/Edit/Write（`9b6e248`）、AskUserQuestion 1–4 题与预览/多选菜单（`1c32aed`、`a88f1bc`、`e36bf98`）、Agent 默认后台（`3dff045`）、CronCreate/Delete/List、Enter/ExitWorktree、NotebookEdit、LSP、ListAgents、ScheduleWakeup、ReportFindings（`1fea3c1` 至 `d874159`）；删除重复的 Delegate/Worker/smart_grep/smart_edit/todo_write（`844d317`、`c9d0153`、`52c1595`）；新增 DeepSeekCode 工具（`064dffe`）。
- [x] 行为：读后改、文件变更后拒绝（`1ef1f26`）；Bash 工作目录跨调用保留，含 PowerShell、Git Bash、cmd（`f477eaa`、`999e6eb`、`1eb27d8`）；超窗口压缩重试与清理旧工具结果（`97df22e`、`ca56527`）；WebFetch 按 prompt 回答、缓存与 URL 规则、新主机询问（`243f7e1`、`e43a786`、`679446b`）；WebSearch 域名过滤（`acbb68a`）；沙箱与域名白名单（`5103267`、`1125aaa`）；MCP HTTP/SSE/WebSocket 与 prompt 命令（`cef6714`、`8b34216`）；`-p` 下 `/compact`（`b2adfe4`）；Remote Control 启动连接（`4ed975e`）；CLI 配置联网搜索与 `/deep-research` 跑通（`4a280f9`、`804d8a3`）；不再询问的规则写入 `.claude/settings.local.json`（`8dbb4b8`）；终端界面、恢复、Shift+Tab 循环与 `--full-access`（`cb13b43`）。
- [x] Desktop 修复：MCP 表单支持远程服务并保留 URL/请求头（`f3eef3b`）；界面不再写 Claude（`4094863`）；更新检查不再访问第三方地址，改由 `DEEPSEEK_CODE_UPDATE_URL` 配置，下载只接受清单给出的带签名安装包（`d4ca5c7`）。
- 未验证：macOS 沙箱（sandbox-exec）；Windows 上 PowerShell 5.1、Git Bash、cmd 的工作目录保留；rustfmt 在本机不可用，新代码未经 rustfmt 检查。用户文档已同步到 `deepseek-code-docs`。

### 暂时不做的任务
- 真实账号 OAuth、用量、fast 模式权限、反馈投递。

## 6. 每项完成后

1. 测试通过 + 必要的 ConPTY 验收 + commit。
2. 在 `temp/claude-code-cli-dev` 记录：commit、fixture/终端场景、结果、失败/取消/持久化覆盖、剩余外部要求；同步勾选清单并更新 `cli-parity-current.md` 对应行。终端逐帧快照（`frame-*`）验收后可删除，保留命名快照与 `raw-ansi.log`。
3. 若用户希望，在 `claude-code-cli-doc`（当前为空）编写面向用户的使用手册（安装、配置、常用命令、远程控制等）。

## 2026-11-02 CLI improvements

- [x] Reap exited shell roots before waiting for inherited output pipes; preserve timeout diagnostics and descendant cleanup.
- [x] Number every tool call, restore transcript numbers and continue numbering after resume.
- [x] Alt+> / Alt+< adjust persisted effort; preserve draft text and paste folding.
- [x] Live /status includes request usage and remaining context, with request-ID deduplication.
- [x] Grep uses workspace-relative paths; shell guidance avoids redundant cd.
- [x] Add `dcl init-env node|python|codegraph|all` and `--dry-run`, with Windows/macOS/Linux installation paths.
- [x] Run actual installers in isolated macOS ARM64 and Linux x86_64 environments; verify repeat initialization and CodeGraph MCP handshake, tool list and source query. CLI 306 passed / 3 ignored on each, shell 5 and search 2 passed.
- [ ] Run actual installers on a clean Windows host. Windows control-flow tests and preview are complete; actual installation is not claimed.

Acceptance evidence: development record `acceptance-20261102.md` and `ux-20261102-K9WURy/assertions.json`. User documentation is in the separate documentation repository under `cli/environment-init.md` and `cli/interface.md`.

## 2026-11-05 用户反馈的 CLI 问题（待修复）

用户在 Windows 上使用 dcl 时反馈。修复已在 Linux 真实终端验证，Windows 尚未验证。回归场景见 `docs/cli-regression-tests.md`。

- [x] 折叠的修改行缺少展开/合并提示。`● #195 Updated temp/inspect_snapshots2.py (+38 -0)` 这类行可以点击展开，但界面上看不出来。Bash 折叠结果已有 `+N lines (click to expand)`（`tool_view.rs`），修改行（`change_view.rs` 的 `collapsed_change`）没有。提示放在现有行内，不新增一行。
- [x] 消息编辑框支持换行输入。新增 `Ctrl+J`（空闲和运行中都可用），运行中也支持行尾 `\` 加 `Enter`。用户所指的具体位置和 Windows 终端的表现待确认。
- [x] 撤回尚未正式发送的消息。输入框为空时按 `↑`，`Tab` 排队和 `Enter` 发送、模型还没读到的消息都回到输入框。此前 `Tab` 排队的消息撤回时不会真正取消，之后仍会发送；排两条以上时较早的消息和工具行还会被挤出显示。
- [x] 恢复会话后，`/continue` 被活动 worker 拦截。

  现象：

  ```text
  ✻ Previous turn · saved 10:07 PM · duration unavailable
  Session restored; waiting for input. Use /continue or send a new task to start the model.
  ❯ /continue
  Error: This session already has an active worker. Use /tasks attach ID or /tasks cancel ID before continuing.
  ```

  当时代码中的问题（用户那次报错的实际原因没有在其机器上确认）：
  - `repl.rs` 的 `show_resumed` 显示这条提示前，没有检查会话是否有活动 worker，提示与随后的报错相互矛盾。
  - `worker_frontend.rs` 的 `single_turn` 会先调用 `recover_session` 清理中断的 worker 状态，`background_commands.rs` 的 `/continue` 没有调用，worker 已退出时也可能被拒绝。
  - 报错要求使用 `/tasks attach ID`，但没有给出 ID。

  修复：`/continue` 先清理中断状态；worker 确实在运行时直接接回；恢复提示按实际状态显示。修复过程中还解决了：前端被杀后几秒内接回失败、进程已退出的任务一直占用会话、崩溃后带 `--permission-mode` 无法恢复、没有结果的工具调用被模型当成成功。
- [ ] 待用户确认是否修改：没有保存耗时记录的旧消息显示 `duration unavailable`（`repl.rs` 的 `completion_for_message`）。

## 2026-11-07 请求失败诊断与有限重试

- [x] 模型请求失败时保留结构化错误（失败阶段、类别、HTTP 状态、实际尝试次数、是否已输出内容、脱敏原因链），重试层与工具循环不再只保留外层文本；摘要、请求快照与任务记录共用同一组次数。
- [x] 发送层与“尚无内容的断流”层共用一份尝试预算，总数不超过 1 加 HTTP 重试配置；退避可取消，取消后不再发起尝试；已输出内容后断流不重放。
- [x] 传输错误识别补上连接建立后被对端关闭的情况；认证、参数、余额与上下文错误仍不重试。
- [x] 内部环境变量前缀统一改为 `DEEPSEEK_CODE_`：后台任务、会话 Worker、批量监督进程、子会话凭据与模型配置。旧前缀不再读取，替换正在运行的任务前需先结束它们。
- 测试与未验证项见相邻开发支持目录的 `api-failure-diagnostics-20261107.md`。

### 审计补齐（同日）

上面四条写成时，脱敏与链路一致性的验收并不完整。后续审计发现并修复了三个可复现的泄漏点，补齐了端到端证据：

- [x] `Cookie` / `Set-Cookie` 整条清除，包含分号分隔的第二项及之后的值。此前正则停在分号，第二项会保留。
- [x] `Proxy-Authorization` 与 `Authorization` 清除完整值，`Basic` 编码凭据不再残留。此前替换认证方案后凭据仍然保留。
- [x] 服务端错误正文不再原样拼接：只读取明确的错误字段，非 JSON 正文保留类型与长度，正文中包含请求结构时改用保守说明。此前整个响应体会进入诊断。
- [x] 端到端一致性：真实 CLI 进程对本地故障服务，核对最终输出、服务端实际请求数与 SQLite 请求快照中的次数三者一致。
- [x] 取消验证：等待响应期间取消与退避期间取消，取消后超过下一次重试时刻仍没有新请求；真实 ConPTY 中按 `Esc` 同样成立。
- [x] 真实 ConPTY 中确认最终文案（阶段、次数、认证含义）与脱敏结果。
- [x] 后台任务链路：真实 supervisor 启动真实 Worker，核对服务端请求数、任务输出（`/tasks show` 内容）与会话快照三者一致；同一链路覆盖认证错误与取消。测试目标 `--test background_failure_diagnostics`。
- [x] Desktop 共享路径：在真实 Tauri WebView2 窗口中确认发送阶段失败与 401 的显示（阶段、类别、次数、脱敏 URL），页面无密钥，Desktop 自身快照与界面一致。
- 记录与截图见相邻开发支持目录的 `api-failure-audit-20261107/`。

## 2026-11-07 项目说明的加载边界

- [x] 说明文件的祖先查找在用户主目录处停止：主目录及以上的同名文件不再作为项目说明加载，用户级说明只来自数据目录。修复了 `lifecycle_events`、`peripheral`、`standalone` 管道输入、`teams` 四个集成用例依赖开发机个人配置的失败。
- 记录见相邻开发支持目录的 `instruction-scope-20261107.md`。

## 2026-11-07 请求诊断第三轮修正

- [x] 远端错误正文只输出固定词汇：可识别类别，或来自固定清单的错误标识（如 `invalid_request_error`）。JSON 错误字段中的自由文本不再进入摘要、日志、事件、快照或会话记录；服务端回显用户提问时同样不上屏。
- [x] 流式结束保留已知 HTTP 状态：`AttemptCounter` 同时记录次数与状态；握手成功但缺少完成标记时输出 `http=200`，不再把 200 写成 API 错误。内置适配器与 Responses 协议保持 `unknown`。
- [x] 两类协议端到端覆盖：OpenAI 与 Anthropic 各有一条“先断连、随后部分内容后正常 EOF”的真实 CLI 用例，以及对应的客户端级用例。
- [x] 修复前台 Worker 栈溢出（`0xc00000fd`）：CLI 主运行时改在 16 MiB 栈线程上运行，调试构建不再在轮次中途崩溃。
- [x] 真实终端与真实 Desktop 窗口验收：确认阶段、类别、次数、HTTP 状态与脱敏结果，页面与 SQLite 快照一致。
- 记录与截图见相邻开发支持目录的 `api-failure-review3-20261107/`。

## 2026-11-07 摘要截断的有限恢复

- [x] `length`（OpenAI）与 `max_tokens`（Anthropic）改为内部结构化截断错误，其他结束原因保持原有失败处理；不依赖错误字符串匹配。
- [x] 首次截断时把该请求的输出预算翻倍重试一次（最高 16384，且不超过模型配置与剩余上下文）；无法提高时直接拆分，避免原样重试。
- [x] 仍截断时按 token 估算二分输入，保持 UTF-8 与原始顺序，分别摘要后合并；最终合并使用同一规则；每条分支最多拆分两层。
- [x] 一次完整压缩最多增加 12 次恢复生成调用，超限后明确失败并保留已完成摘要；仅正常结束并通过校验的摘要进入缓存。（已被 2026-11-08 的按分段额度和合并池策略取代，保留为历史记录）
- [x] 恢复结果同时写入原请求键，重启或重试可跳过已完成的恢复；原始历史与压缩边界的事务提交、取消和并发检查不变。
- 测试：`compaction_truncation`（提额、拆分合并、恢复上限、跨失败复用）与原有 `compaction_overflow`；场景见 `cli-regression-tests.md` 的 L。

### 收尾（同日）

- [x] 提高输出预算后按新预算重建对应阶段的提示词：分段与合并以实际预算的一半为目标，最终摘要同样；新请求的缓存键使用实际提示词与预算，成功结果仍写回原请求键。
- [x] 恢复合并与分段、最终摘要共用同一安全输入预算检查；超出时在第 12 次恢复额度之外立即明确终止，不发送超限请求，也不增加新的拆分层。（额度的具体数字已被 2026-11-08 策略取代；同一安全线检查保留，并在 2026-11-08 补上单次 64K 上限）
- [x] 记录最后一次实际发送的输出预算：深度耗尽、恢复额度耗尽与输入无法容纳分别给出不同原因，非截断失败保留原始错误链并附上阶段、预算、恢复次数和历史保留说明。
- [x] 手动 `/compact` 补上与自动压缩相同的逐段进度显示，修复文档已描述但界面缺失的进度行。
- [x] 新增验收覆盖：首段 4096 提额、合并超出安全线、最后实际预算、非截断立即停止、关闭并重开数据库与引擎后的子摘要复用、缓存中不存在截断文本、并发修改历史拒绝提交、局部摘要截断、超长工具结果与中文 emoji 跨请求保持。
- [x] 真实 ConPTY 验收：隔离会话构造“超大历史、两段摘要、首段截断”，自动压缩与手动 `/compact` 都显示恢复进度并完成，压缩后的原请求只执行一次且携带摘要而非原始长历史。脚本与结果见相邻开发支持目录的 `compaction-recovery-20261107/`。

### 第二轮复核收尾（同日）

- [x] 分段规划失败（窗口不足以容纳提示词与输出预算）同样报告阶段、拟请求预算、恢复次数、未发送请求与原始历史保留；此前只输出一句窗口过小。
- [x] 合并输入超出安全预算时保留 `merge` 阶段名，并分别给出拟请求预算与最后一次实际发送的预算，避免把子摘要提额后的数字当作合并请求的预算。
- [x] 新增合并再次截断的验收：原始分段截断、两个子摘要成功、合并截断后提额成功（OpenAI 与 Anthropic 各一条）；合并持续截断时停在深度上限，错误报告阶段、两个预算与恢复次数。
- [x] 新增恢复失败后的合并重试用例：成功的子摘要从检查点复用，不再请求模型；只有完整压缩成功后才替换历史。
- [x] 修复请求体被补齐空字段的问题：`normalize_chat` 与 `normalize_anthropic` 改用 `get_mut`，不再给没有 `tool_calls`、`content` 的消息插入 `null`。此前实际请求比引擎估算多出字节，分段可能略微超出安全线。
- [x] 终端验收脚本改用与引擎同口径的完整请求体估算（含系统指令、按消息分组向上取整），并把“两段”改为结构化断言：原始分段恰为 2、恢复子段恰为 2、合并在子段之后，并核对 `fragment 1/2`、`fragment 2/2` 两个终端标签。
- [x] 复跑：`compaction_truncation` 16 项、`compaction_overflow` 5 项（1 项按设计忽略）、`generation` 单测 5 项、CLI 全部 31 个目标、Desktop `--all-targets` 检查通过；真实 ConPTY 两个场景 PASS。

### 第三轮复核收尾（同日）

- [x] 提额分支在恢复额度耗尽时改用本次本应发送的提额预算报告：此前传入的是刚被截断那次请求的预算，错误因此写成“下一次 4096”而实际会是 8192。普通恢复子请求仍报告当前请求预算。
- [x] 新增 `a_spent_allowance_names_the_raised_budget_it_did_not_send`：四个大分段各自分出两个子摘要，前三次合并用尽 12 次恢复额度，第四次合并在 4096 被截断；断言错误报告 `merge` 阶段、12/12 次恢复、下一次拟请求 8192、最后一次实际发送 4096、未发送任何 8192 请求且历史未变。（该用例随 2026-11-08 的额度策略改写，现为 `a_failing_fragment_keeps_the_others_and_spends_only_its_own_allowance`）
- [x] 复跑：`compaction_truncation` 17 项、`compaction_overflow` 5 项（1 项按设计忽略）、`native_engine::` 单测 195 项（1 项忽略）、CLI 构建通过；`rustfmt --check` 与 `git diff --check` 通过。

## 2026-11-08 超长历史的分段上限、每段额度与分层合并

背景：约 85 万 tokens 的历史在一次请求中整体发送，只留 8192 输出，篇幅目标约 4096。输入规模与可用的输出严重失衡，模型很容易以 `length` 提前结束（是否截断取决于模型，不是必然结果；本地 mock 用输入超过 100k 即截断来稳定复现）。同时整次压缩共用 12 次恢复额度，第一个大段就耗尽，后续分段无法发送。

- [x] 单次摘要请求的完整输入上限为 64,000 tokens（含系统指令与阶段提示词），同时受窗口 90% 安全线约束；超过即先分段，即使模型窗口能装下整段历史。
- [x] 分段与合并规划优先为一次可行的提额预留输入空间；窗口过小时退回初始输出预算。
- [x] 篇幅目标在首次请求时固定：分段与合并约 2048 tokens、最终摘要约 4096 tokens；提额只增加可用空间，不再同步放大目标。
- [x] 恢复额度按初始分段数分配：`R = min(128, 8 × N + 16)`，合并预留 16，其余均分且每段最多 8 次；成功分段的剩余额度转入合并池，每个中间合并或最终摘要节点最多使用 `min(合并池剩余, 8)`。
- [x] 某段额度或拆分深度耗尽改为内部结构化错误，只在该段边界被捕获；其它独立分段继续完成并写入缓存，存在失败段时不提交，错误列出失败分段、预算、次数与原因。
- [x] 中间合并改为分层：按时间顺序每组最多 8 份摘要并受同一输入上限约束，单剩余项进入下一层；摘要数量无法减少或两份摘要都放不下时明确失败，不循环。
- [x] 检查点指纹版本升到 2，旧分段策略留下的中间结果不再被复用；请求键仍区分实际提示词与输出预算。
- [x] 进度与日志补充分段/合并层/复用计数，以及每次请求的输入估算、输出预算、结束原因与用量。
- [x] 测试：`compaction_truncation` 19 项（含约 85 万 tokens 合成历史、单段失败隔离、分层合并、合并池余额报告）；`compaction_overflow` 5 项（1 项按设计忽略）；`generation` 单测 9 项；`services::compaction` 单测 10 项。
- [x] 已知既有失败（与本次无关，工作区其它未提交改动导致）：`crates/core/tests/native_compaction.rs` 的 3 项与 `crates/cli/tests/standalone.rs` 的 11 项均报 `Background control channel does not own this session`；已用 `git stash` 排除本次改动后复现。

## 2026-11-08 审计复核：恢复合并 64K、worker 环境隔离与验收证据

审计报告位于相邻开发支持目录的 `session-audit-901f0e85-20261108/`。本轮处理了其中五个任务。

- [x] 恢复合并也受 64K 单次输入上限约束：`text_fits` 同时检查窗口安全线与 `MAX_INPUT_TOKENS`；超限的合并请求不发送，只让该分段失败。缓存命中也先过同一检查，旧策略写入的检查点无法把超限请求带回来。回归 `a_recovery_merge_above_the_single_request_ceiling_is_never_sent`（两协议，约 79k 合并输入，1M 窗口）在修复前失败、修复后通过。
- [x] 普通子进程不再继承 worker 控制变量：`background_sessions::strip_internal_env` 在 Bash shell（前台、后台、分离、沙箱、WSL）、hook 命令和 MCP stdio 启动处移除 `DEEPSEEK_CODE_BACKGROUND_JOB`/`_CONTROL`；受管 worker 仍由 supervisor 显式注入自己的控制目录。新增 `crates/core/tests/worker_environment.rs`：测试自身设置这两个变量后，Bash 子进程读到空值而普通用户变量仍在；设置 `DCL_ACCEPTANCE_COMMAND` 时还会把被测命令（如 `native_compaction`、`standalone` 测试二进制）跑过同一 shell，验证真实链路。
- [x] 纠正归因：此前把 `native_compaction` 3 项、`standalone` 11 项失败归为“其他未提交改动”，实际是宿主的 `DEEPSEEK_CODE_BACKGROUND_CONTROL` 被 Bash 子进程继承。A/B 证据：去掉 strip 时验收命令失败，加回后通过；`standalone` 干净环境 12/12 通过。
- [x] `standalone` 两项权限断言失败的原因是文案回归：受限 agent 调用未授权工具时，`loading_violation` 走“该工具会在下一次请求可用”的分支，而不是说明它不属于该 agent。修复为在检查额度/加载策略前先判断 `allowed_tools`，返回 `{name} is not allowed for this agent.`；工具仍然没有执行（测试断言 `created.txt` 不存在），不是权限绕过。
- [x] 旧/新策略同规模实测（同一 mock、同一约 85 万 tokens 历史、1M 窗口、16,384 输出上限）：旧策略 6 次请求、最大输入 880,271 tokens、累计输入 3,081,146、重复发送 3 次、约 2.8 秒后因深度上限失败；新策略 18 次请求、最大输入 64,000、累计输入 885,925、无重复发送、约 2.2 秒成功并提交边界。日志见 `compaction-budget-20261108/`。
- [x] 补行为测试：约 80 万 tokens 的单段失败隔离与重试缓存复用；长摘要导致每组 5 份、单项传递到下一层；缓存写入失败立即终止且不提交；合并节点额度上限（`node_quota`）与分段余额转入合并池。
- [x] 日志补齐：缓存命中（含提额命中与子摘要命中）计入复用并写 tracing；恢复请求记录已用/剩余额度；分段成功/失败记录完成数与合并池余额；单次最终摘要路径也输出完成统计。
- [x] ConPTY：脚本改为每次运行独立目录（`--out` 或 `runs/<时间>-<随机>`，含 `run.json`/`results.json`，非空目录拒绝）；新增窗口 100,000 的 `large`/`large-manual` 场景，历史约 86k tokens，整段能进 90% 安全线却超过 64K 上限，64K 分段连续两次触顶后由 32K 子段完成。四个场景（14K auto/manual、100K large/large-manual）全部 PASS。
- [x] 回归入口：`session-audit-901f0e85-20261108/run-regression.py` 逐条记录开始时间、耗时、真实退出码和独立日志，整批结果取 AND；自测“第一条 exit 101、末尾 exit 0”仍判失败。
- [x] 文档：`cli-regression-tests.md` 的 L 节与改动范围映射、用户文档“其他协议的超长上下文摘要”、`desktop-regression-tests.md` 新增 D3 节均已同步；旧的“整次 12 次全局额度”记录已标注被新策略取代，并修正“85 万输入配 8192 输出必然截断”的绝对表述。
- 未验证：真实供应商会话恢复（需单独授权，且不能复用原失败任务）；Desktop 窗口内的 D3 场景本轮未做界面验收。
- 已知：本轮对 `crates/core/src/services/background_sessions.rs`、`crates/core/src/tools/mod.rs` 运行了 rustfmt，这两个文件此前已有未提交改动，因此 diff 中包含少量与本任务无关的换行整理。
