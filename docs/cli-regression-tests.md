# dcl 回归测试清单

修改 `crates/cli`、`crates/core` 后，agent 必须读取本文，确认改动没有影响其他功能。修改完成的标准不是“新功能能用”，而是下面对应的检查都做过，并如实汇报结果。

## 使用步骤

1. 用 `git diff --name-only` 确定改动范围，在“按改动范围选择场景”中找到要跑的场景。
2. 跑“必跑命令”。出现失败时，先在未修改的代码上跑同一个测试，区分是这次引入的还是原有问题。
3. 在真实终端里跑选中的场景。只改了不影响界面的内部逻辑时，可以只跑相关测试，但要在汇报中说明。
4. 新修的问题要在本文补一个场景，避免以后再次出现。
5. 汇报时说明跑了什么、结果如何、哪些没有验证（例如 Windows）。没跑的不能写成通过。

本文中 `*-2026MMDD/` 形式的脚本与证据目录原在相邻的 `dev-support/deepseek-code`，已在 2026-11-10 清理中删除（记录见该目录的 `cleanup-20261110/README.md`）。这些条目保留为可重跑的步骤和验收口径，`cargo test` 目标不受影响；涉及已删脚本的项要按条目描述重新实现，不能引用旧结果作为本次通过。

## 必跑命令

Linux 上先加载本机构建环境（`source ~/.local/dcl-build-env.sh`）。Cargo 命令串行执行。

```bash
git diff --check
cargo test --locked --no-fail-fast -p deepseek-code-core --lib --tests
cargo test --locked --no-fail-fast -p deepseek-code-cli
cargo check --locked -p deepseek-code-desktop --all-targets
```

加 `--no-fail-fast`，否则第一个失败的测试程序之后的测试都不会运行，结果会被漏掉。

Windows 调试构建需要较大的栈：工具调用会嵌套很深的 future，主线程默认栈（Windows 约 1 MB）会在轮次中途溢出，进程直接以 `0xc00000fd` 退出，本轮什么都不会报告。CLI 现在把主运行时放在 16 MiB 栈的线程上，直接启动构建产物或跑 ConPTY 验收不再需要额外设置 `RUST_MIN_STACK`；`cargo test` 仍由 `.cargo/config.toml` 设置该变量。集成测试的临时工作区应放在没有祖先项目说明文件的独立目录，避免用户目录中的配置或说明污染本地 fixture；只在测试进程中设置 `TEMP` / `TMP`，不要修改系统环境变量。

在 dcl 的后台任务或 Worker 内部执行测试时，测试进程会继承 `DEEPSEEK_CODE_BACKGROUND_CONTROL` 和 `DEEPSEEK_CODE_BACKGROUND_JOB`。这两个变量会让新建的临时会话被判定为“不属于该控制通道”，大量与会话、委派、目标相关的测试因此失败并报 `Background control channel does not own this session`。只在这条测试命令的子进程中清除它们，例如 `env -u DEEPSEEK_CODE_BACKGROUND_CONTROL -u DEEPSEEK_CODE_BACKGROUND_JOB cargo test ...`，不要修改系统环境或正在运行的任务。出现这个错误时先按本段处理，再判断是否真的有回归。

同一宿主环境还会带出 `DEEPSEEK_CODE_WORKER_MODEL`、`DEEPSEEK_CODE_WORKER_AUTO_ROUTE` 和 `DEEPSEEK_CODE_CHILD_API_KEY`。`batch` 用例启动的 Worker 会继承这些变量，宿主的模型快照与用例自己指定的 `--model` 不一致，Worker 直接报 `Worker model snapshot does not match the selected model` 退出，表现为 `status: "failed"` 而不是 `cancelled`/`completed`。跑测试时连同这三个变量一起清除：`env -u DEEPSEEK_CODE_WORKER_MODEL -u DEEPSEEK_CODE_WORKER_AUTO_ROUTE -u DEEPSEEK_CODE_CHILD_API_KEY ...`。

## 已知失败

Windows 上，`tools::linux_env::tests::unpack_copies_hard_links_keeps_symlinks_and_skips_devices` 的 `read_link` 断言失败：根文件系统解包只在 Unix 创建符号链接，该测试却在所有平台检查链接。2026-11-06 合并检查从合并前的 `main` 提取原始模块，单独编译并重跑同一测试，复现相同的文件不存在错误。记录位于相邻开发支持目录的 `merge-desktop-main-20261106/core-baseline-test.log`。

测试进程会继承执行它的宿主会话环境。`DEEPSEEK_CODE_DATA_DIR` 会让独立进程用例写到宿主的数据目录，运行前先清除。

有少数用例依赖真实的进程启动或固定超时，在机器同时进行编译或其他测试时会偶发超时：`services::hook_progress::tests::real_hook_success_failure_timeout_and_cancel_clear_status`（钩子 2 秒超时）、`crates/cli/tests/session_worker.rs` 中崩溃恢复相关用例（重启 Worker 的等待窗口）。这类失败应先单独重跑同一用例确认；单独通过且代码未涉及时按负载问题处理，不要为了让它通过而放宽产品超时。2026-11-07 的两次全量运行各出现一次，均在单独重跑时通过。

2026-11-07 之前，`lifecycle_events`、`peripheral`、`standalone` 的管道输入用例和 `teams` 在本机失败：它们断言整段消息内容，而用户主目录下的 `~/.claude/CLAUDE.md` 被当作项目说明注入首个用户消息。现在说明文件的祖先查找在用户主目录处停止，用户级说明只来自数据目录，因此这些用例不再依赖开发机的个人配置。若再次出现类似失败，先检查断言中是否多出了某段说明文件内容，再确认该文件是否位于项目目录与用户主目录之间。

出现其他失败时，先在未修改的代码上跑同一个测试，再判断是否由本次改动引入。不要将有已知失败的整套测试报告为通过。

## 重试测试使用受控时钟

`empty_disconnect_is_bounded_and_can_be_cancelled_during_backoff` 和 `persistent_send_failures_stop_at_the_configured_attempt_limit` 保留真实的本地 HTTP 通信，使用暂停的 Tokio 时钟验证完整退避过程。测试只在收到 `retry_wait` 事件并检查请求次数、等待时长后推进时间；每次在截止前 1 毫秒轮询请求，确认不会提前重试，再推进到截止时间。七次等待仍为 2、4、8、16、32、64、128 秒，虚拟累计耗时必须为 254 秒，实际执行无需等待这些时间。

共享的 `manual_clock` 测试辅助代码在网络通信期间通过阻塞任务抑制 Tokio 自动推进时钟，避免本地 HTTP 尚未响应时提前触发请求超时；独立的真实时间看门狗将异常等待限制在 10 秒内，正常结束或 panic 时释放阻塞任务。不要只添加 `start_paused` 而让网络和请求超时自行竞争，也不要缩短产品配置中的重试间隔。

```bash
cargo test --locked -p deepseek-code-core --lib native_engine::stream_retry::tests::
cargo test --locked -p deepseek-code-core --test request_failure --test reconnect_policy
```

检查这两个原有慢测试确实执行并通过，且不再各自等待约四分钟。保留流输出不重复、八次请求上限、错误阶段与脱敏断言；取消场景必须先观察第一次退避，推进 50 毫秒后丢弃流，再推进超过下一次重试截止时间，确认仍只有一次 HTTP 请求。`reconnect_policy` 继续独立验证重连事件和服务端 `Retry-After` 行为。单元测试与本地 HTTP 测试的通过不代表真实提供商或其他平台已经验收。

## 测试进程不应操作控制终端

`cargo test -p deepseek-code-cli` 会运行 `--bin dcl` 的单元测试。这些用例不能查询光标位置，也不能切换 raw mode：`crossterm::cursor::position` 会自己把 `/dev/tty` 切到 raw mode，进程处在后台进程组时内核发送 `SIGTTOU`，整个进程组停止，cargo 会一直等到被强杀；在开发者终端里直接跑 `cargo test` 时，无参 `/config` 还会进入真实 picker 等待按键。`script` 默认创建的 pty 没有 winsize，`crossterm::terminal::size()` 会返回 `Ok((0, 0))`，绕过 `unwrap_or((80, 24))`，换行宽度变成 0。因此：

- 光标查询统一走 `crate::terminal::cursor_position()`，未处于 raw mode 时返回 `NotConnected`，调用方保留原有的回退行号。
- 终端尺寸统一走 `crate::terminal::size()`，尺寸为 0 时回退到 80×24。
- `TerminalInput` 由测试夹具提供答案（`is_fixture()`）时，无参 `/config`、`/cost`、`/status` 走文本路径，不进入 picker。

验证方式：

```bash
cargo test --locked -p deepseek-code-cli --bin dcl
script -qec "stty rows 40 cols 100 && cargo test --locked -p deepseek-code-cli --bin dcl" /dev/null
script -qec "cargo test --locked -p deepseek-code-cli --bin dcl" /dev/null
grep -rn "crossterm::cursor::position" crates/cli/src --include=*.rs
grep -rn "crossterm::terminal::size\|[^:]terminal::size()" crates/cli/src --include=*.rs
```

2026-11-09：修复前第一条命令会停住（进程状态 `Tl`，`wchan=do_signal_stop`），有窗口大小的 pty 里曾停在 Settings 菜单等待输入；修复后前两条命令都在数秒内完成，`terminal::cursor_tests::cursor_position_is_unavailable_without_raw_mode` 与 `terminal::input::tests::fixture_answers_are_non_interactive` 通过。第三条命令使用 `script` 默认的 0×0 pty：修复前 29 个用例因宽度为 0 失败，修复后应与第一条一致。光标 grep 只应命中 `crates/cli/src/terminal.rs` 里的辅助函数本身和它的注释；尺寸 grep 只应命中 `crates/cli/src/terminal.rs` 中 `size()` 的实现（`remote_host.rs` 和 `remote_client.rs` 的 `terminal_size()` 已委托给它）。

0×0 pty 下还要人工执行一次 `/status`：`script -qec "stty rows 0 cols 0 && dcl" /dev/null`，进入会话后输入 `/status`，确认报告按 80 列换行，没有宽度为 0 的退化。

这些渲染用例按宿主终端宽度换行。宿主控制终端尺寸异常（例如只剩 1×2）时，`cargo test --bin dcl` 会因宽度过小而失败；这是环境问题，不是卡死，改用第二条带 `stty` 的命令或 `setsid` 运行即可。

## 执行类测试的工具前置条件

团队、会话消息、持久 Worker、工作树用例在隔离数据目录中用 `tests/support/tool_loading.rs` 显式开启其执行所需的非核心工具：Agent、SendMessage、LSP 或团队工具。用例仍须验证真实工具执行、子会话、目录权限、取消和恢复结果，不能只断言进程成功退出。默认工具加载策略保持不变，发现与恢复加载资格由 core 的工具加载策略测试独立覆盖。

修改工具默认值或这些夹具后，执行：

```bash
cargo test --locked --no-fail-fast -p deepseek-code-cli --test teams --test session_messages --test session_worker --test worktree_startup
```

若出现 `is not loaded. Call ToolSearch`，先确认场景是否需要显式加载工具或增加模型搜索步骤；不要把未执行工具导致的后续空结果误判为 Worker 或消息传递失效。

## 真实终端的测试方法

真实终端指在伪终端里启动 `dcl`，发送按键，再检查屏幕内容。Linux 上使用开发目录的工具：

- `runs/ux/drive.py`：按 JSON 步骤驱动 `dcl`，用开发目录 `.venv` 里的 Python 运行，才能得到真实光标位置（鼠标点击依赖它）。
- `runs/ux/snap.sh`：把录制内容渲染成最终屏幕。
- `runs/ux/dcl.sh`：用独立的 `DATA`、`PROJ` 目录启动。测试前先 `cargo build --locked -p deepseek-code-cli`，`cargo test` 不会更新它运行的程序。

注意事项：

- 每轮用新的数据目录，测试结束后结束自己启动的进程。
- `until` 等待的文字如果也出现在自己输入的提示词里，会立刻匹配成功，这时后面的输入会变成运行中发送。等待模型回复时要选提示词里没有的文字，或用固定等待。
- 不要用 `pkill -f "某段文字"`，它会连当前 shell 一起结束。
- 用外部 `kill` 命令结束进程组时，负数 PID 前必须加 `--`（`kill -9 -- -123456`）。Debian 的 procps `kill -9 -123456` 会被解析成 `kill(-1, SIGKILL)`，结束当前用户的全部进程，包括 tmux 和正在运行的 dcl。代码里优先直接调用 `rustix::process::kill_process_group`；`external_kill_never_takes_a_negative_pid_without_separator` 会检查源码。
- 怀疑测试会结束无关进程时，用 `unshare --map-current-user -Upf --mount-proc` 在独立 PID 命名空间里运行，信号到不了命名空间外。
- `--resume` 时如果 worker 还活着，不能带 `--permission-mode`，这是有意的限制。

## 场景

### 文件锁的平台一致性

共用引擎的文件锁通过 `fs::try_lock_file`、`fs::lock_file` 获取，短暂竞争使用 `fs::try_lock_briefly`。Android 使用系统 `flock`，其他平台沿用标准库；关闭持有文件后由系统释放所有权。

1. `cargo test --locked -p deepseek-code-core --lib fs::file_lock`：首次获取成功，第二个独立句柄返回 `WouldBlock`，失败的探测不会释放原锁，持有者关闭后可以重新获取。
2. 在 Android 真机运行同一锁测试，并用首次创建的会话发消息。真实存在的竞争者仍被拒绝，正常会话可以开始问答。
3. 回归会话执行租约、presence、一次性 hook、设置写入和 worker 恢复测试，确认调用方统一使用共用锁入口；不要将不支持的锁调用等同于已有执行者。

### A. 输入框换行

1. 空闲时输入文字，按 `Ctrl+J`，再输入文字：显示两行，没有发送。
2. 空闲时按 `Shift+Enter`、`Alt+Enter`、行尾 `\` 加 `Enter`：都换行。
3. 模型运行中重复以上操作：换行且不发送，输入框真正分行显示，内容很长时最多显示 8 行并随光标滚动。
4. 多行内容中按 `↑`/`↓` 在行间移动光标；到第一行或最后一行时才回到历史记录。
5. 普通 `Enter` 仍然发送，斜杠菜单打开时 `Enter` 仍然选择菜单项。
6. Windows 上 `Shift+Enter` 依赖输入读取方式：ConPTY 按 Win32 输入记录发送带 `SHIFT` 的回车，而 `ENABLE_VIRTUAL_TERMINAL_INPUT` 会把它重新编码成普通回车，直接发送。`crates/cli/src/terminal/paste_input.rs` 的 Windows 读取器保持记录模式，不要改回 VT 输入；改动后要在真实 ConPTY 里复测本条和多行粘贴。

### A2. 多行粘贴

1. 用 `Shift+Insert` 或终端自己的粘贴快捷键，把一份多行文本（例如 30 行、约 800 字）粘贴到输入框：输入框显示折叠摘要，没有发送任何内容。
2. 再按一次 `Enter`：整段文本作为一条消息发出，内容与原文一致，包括空行、中文和换行位置。
3. 同一段文本分别按“一次性写入”和“分块写入、块间有停顿”各测一次，结果相同。
4. 模型运行中重复第 1 步：文本进入草稿，同样不发送。
5. Windows 说明：ConPTY 把粘贴中的换行（U+000A）报成 `VK_RETURN` 加 `CONTROL` 状态，读成按键就是 Ctrl+Enter。`crates/cli/src/terminal/paste_input.rs` 把这种记录还原成粘贴文本；真实按下的 Enter 带 `\r`，不受影响。改动后要在真实 ConPTY 里复测本条和场景 A 第 6 条。

### B. 运行中排队和撤回

让模型运行一个较长的命令（如 `sleep 40`），在此期间操作：

1. 输入消息按 `Tab`：显示在工作区下方，输入框提示 `Press up to edit queued messages`。
2. 再输入消息按 `Enter`：两条都显示，运行中的工具行也还在。
3. 排三条消息：三条都显示，工具行也还在。
4. 输入框为空时按 `↑`：所有未读消息按发送顺序回到输入框，排队区清空。
5. 清空输入框后等这一轮结束：撤回的消息没有发给模型（数据库 `messages` 表里没有对应的用户消息）。
6. 不撤回时：`Enter` 的消息在下一个工具结果后交给模型，`Tab` 的消息在这一轮结束后作为下一条发送。
7. 输入框有内容时按 `↑`：仍然是历史记录，不撤回。
8. 后台任务通知不会被 `↑` 取到输入框。

### C. 点击展开和合并

1. 编辑文件后，修改行显示 `● #N Updated 路径 (+x -y) (click to expand)` 和最多 4 行 diff。
2. 点击后显示完整 diff，末尾有 `(click to collapse)`；再点击回到折叠状态。
3. Bash 结果超过 3 行时显示 `+N lines (click to expand)`，展开后可收起。
4. `--resume` 后，修改行的显示和点击效果与上面相同。
5. 思考内容 `Thought ... (click to expand)` 和较长的用户消息也能展开、收起。
6. `Ctrl+O` 的详细记录不受影响，也不出现点击提示。

### D. 中途关闭后恢复

这些场景都要在关闭后立刻恢复一次，因为被杀的前端留下的占用记录要几秒后才过期。

1. **只关前端，worker 还活着**：`--resume` 显示任务仍在运行和任务 ID；`/continue` 立刻接回同一个任务，命令不重新执行，结束后输出结果。
2. **前端和 worker 都被杀**：`--resume` 提示已恢复中断状态；`/continue` 开始新请求。模型收到的结果说明工具没有执行完，不能把它当成成功。原来的任务记录变为 `failed`，原请求不重放。
3. **带 `--permission-mode` 恢复已崩溃的会话**：不再报告 `active background work`。
3a. **只有 supervisor 被杀**：worker 主进程约 5 秒内自行退出，之后可以恢复。
4. **worker 长时间无响应但进程还在**：`/continue` 不接回，提示用 `/tasks show ID` 查看，并给出任务 ID。
5. **会话被占用时发送普通消息**：报错包含任务 ID 和 `/continue`，消息没有提交。
6. **没有活动任务**：显示 `Session restored; waiting for input`，`/continue` 正常继续，通知只在继续后才唤醒模型。
7. 恢复后的历史里，没有结果的工具调用显示 `No result recorded`。

### F. /tasks

有两个后台命令时输入 `/tasks`：

1. 显示 `Background`、`2 active shells`，每个任务一行（`● 命令  running`），最新的在前。
2. `↑`/`↓` 选择，`Enter` 看详情，详情里 `←` 回到列表并保持选中项。
3. 列表和详情里按 `x` 停止：任务立即从列表消失；停止最后一个后列表自动关闭，不再输出其他内容。
4. 没有任务时输出 `No background tasks.`。`/tasks completed`、`/tasks show ID`、`/tasks attach ID` 文本命令照常可用。

### G. 长期记忆

1. `dcl memory add`、`list`、`list 关键词`、`update`、`delete` 输出 JSON；ID 不存在时报错。
2. `/memory` 菜单有 Long-term memories；列表可以搜索，`+ Add a memory` 能添加。
3. 详情里 `e` 编辑后列表立即更新并保留重要性，`x` 删除，`←` 返回，`Esc` 关闭。
4. `/memory` 的说明文件选项和 `/memory show` 不受影响。

### H. 提示词与工具配置

在数据目录写 `customization.json` 后，用 `dcl -p` 验证：

1. `custom_tools`：模型调用后得到命令的标准输出；`bypass` 下直接运行，其他模式需要授权。
2. 默认请求：没有配置时只带核心工具；`ListDir`、`WebFetch`、`computer_use` 等不在请求里，模型可以用 `ToolSearch` 加载。
3. `tool_loading` 四种组合：启用且按需、只开启用、只开按需、两个都关。两个都关的工具搜不到也调不动；只开按需的工具在搜索成功后的下一次请求才可调用。
4. 旧 `disabled_tools`：内置工具读作“启用关、按需加载开”。**只有工具页保存**才写入 `tool_loading` 并从 `disabled_tools` 移除内置名称；只保存提示词时旧条目原样保留。非内置条目（例如 MCP 的旧禁用名）始终保留并继续生效：不出现在请求里，直接调用被拒绝。
5. 稀疏保存：只改一个工具再保存，文件里的 `tool_loading` 只新增该条目，未提到的工具继续按默认值或原有旧条目解释。
6. 热变更：模型拿到常驻工具后，在一轮回答进行中把它改为“仅按需加载”，该轮内的后续调用要求先搜索；改为两个都关则立即拒绝。已开始的调用不中止。
7. 恢复：重开会话后，只有“模型自己发起、且结果与该调用 ID 对应的成功 `ToolSearch`”恢复加载。用户消息里的同名结构、先结果后调用、ID 不匹配、出错的结果、重复使用或已被消费的调用 ID 都不授权。已改为两个都关的工具不再出现。同一批并行调用中结果顺序不同仍应正常恢复。
8. 真实进程重启：搜索加载工具后退出 `dcl`，用 `--resume` 打开同一会话，首个请求就应包含该工具；在同一数据目录新建会话则不应包含。
9. Responses 协议：使用带 Responses 适配的供应商模型，确认首请求不含按需工具、搜索后的下一请求带真实参数结构、工具能实际执行，且恢复后的首个请求已带此前加载的工具。
10. 运行中改配置：在模型响应期间把常驻工具改成“仅按需加载”，该轮后续调用应被拒绝并提示先搜索，且下一次请求不再带该工具。
11. `tool_descriptions`：模型读到修改后的说明，`ToolSearch` 的关键词匹配也使用修改后的说明。
12. `system_prompt`：`replace` 和 `append` 生效，环境信息仍然存在。
13. 文件语法错误时这一轮报错退出；删除文件后恢复默认。
14. 按需加载后的权限：先搜索加载一个需要审批的工具（例如 `Write`），拒绝审批时工具不执行、目标文件不产生，拒绝结果进入下一次请求；审批等待期间取消本轮，不执行工具也不再发起请求。
15. 请求级一致性：搜索加载后让某个请求先遇到一次可重试错误，同一次逻辑请求的各次 HTTP 尝试携带相同的工具列表；请求快照的工具条目应逐项包含实际发送的名称、说明和参数 schema，token 估算应由该请求体重算得到相同结果；只有下一逻辑请求才按最新加载集合生成新列表，且加载后 schema 字节数确实增长。覆盖 OpenAI 与 Anthropic，并确认 Responses 适配结果。
16. 实际压缩：先成功搜索加载一个按需工具，执行一次真实压缩后重开会话，首个请求不再带该工具（摘要不能授权），模型重新搜索后下一请求恢复。
17. Responses 真实进程重启：用隔离数据目录 `config.json` 配置 Responses 适配供应商（模型 `api` 为 `openai-responses`，内置供应商 ID 为 `openai`），真实 `dcl` 进程搜索加载后退出，`--resume` 新进程的首个 Responses 请求应包含此前加载的工具。不要用 `--base-url` 覆盖，它会清除内置供应商 ID。
18. 命令改开关：在隔离数据目录执行 `dcl config set tools.<name>.enabled|on_demand true|false`，`dcl config show` 的 `tools` 项与文件内容一致；下一次模型请求的工具集合按新开关变化（两个都关的工具不出现也搜不到，只开启用的工具直接出现在请求里）。同一次写入把 `disabled_tools` 里的内置名折算成开关，非内置名保留。交互终端执行 `/tools`，用 `Space` 改“启用”、`Tab` 改“按需加载”，关闭面板后文件与面板一致；非终端下 `/tools` 打印文本清单。`crates/cli/src/tool_settings.rs` 的单元测试覆盖只改一个开关、双关、迁移保留非内置名和非法输入。

19. 搜索匹配：多个关键词分开匹配，`_`、`-` 视为空格，例如 `git diff` 首先加载 `git_diff`；任一关键词都匹配不到时不加载；`select:git_status,ListDir` 只按名称精确加载，并保持给出的顺序。 完整工具名支持逗号、空白和混合分隔（例如 `WebSearch WebFetch`、`WebSearch,WebFetch`），忽略大小写、去重并保持顺序；`select:` 同样支持两种分隔。名称识别基于完整目录，禁用、已暴露及代理未允许的工具仍不加载。

自动回归：`cargo test --locked -p deepseek-code-core --lib tool_loading_tests`。
provider mock 直接驱动公开 executor，覆盖 Anthropic、Chat Completions、Responses：
首请求不带按需工具，ToolSearch 成功后下一 HTTP 请求包含真实 schema 并执行 ListDir；
在途时双关和重新启用更新后续请求、拒绝失效调用；损坏配置阻止下一 HTTP 请求；
同一逻辑请求的 HTTP 重试固定工具定义，快照与实际请求工具集合一致。
恢复回归另从关闭后重新打开的 SQLite 读取成功 ToolSearch 工具行，传入只有显示文本的历史；
验证首个真实请求恢复工具、当前开关及代理限制仍生效，错误/非助手/其他会话/压缩边界前记录不授予加载资格。
这些 core 结果不能代替终端 `/tools` 同会话联动或 Desktop 原生界面验收。

### I. 定时任务

1. 让模型创建每分钟运行的 `CronCreate` 任务：空闲时按时触发，显示 `⏰ Scheduled:` 并得到回答。
2. 输入框有内容或正在运行时不打断，空闲后再触发。
3. 恢复的会话在 `/continue` 或发送新消息前不触发。
4. 同一项目开两个会话时，持久任务只在一个会话触发。
5. `/loop 1m 提示词`、`/loop stop`、`ScheduleWakeup` 可用。

### E. 基础流程

1. 启动到输入框，发送消息，收到回答。
2. 工具审批：`ask` 模式下出现菜单，允许和拒绝都生效；`Shift+Tab` 能切换模式。
3. `Esc` 中断当前一轮，已有对话保留。
4. `/background` 分离后 `/tasks attach ID` 能接回，`/tasks cancel ID` 能停止。
5. `/resume` 列表、`/rewind`、`/compact`、`/clear` 可用。
6. `--print` 和 `--output-format stream-json` 正常输出（`crates/cli/tests/sdk.rs` 覆盖 SDK 协议）。
7. Java SDK 的端到端测试通过（修改 SDK 协议时跑，方法见 Java SDK 仓库的 README）。

### M. OpenCode Go 接入

使用 `OPENCODE_API_KEY` 与独立的临时数据目录；不要把密钥写进配置、脚本或记录，测试完删除临时目录。

OpenCode Go 用四种协议提供模型，模型地址和协议都来自内置目录的 `opencode-go` 条目：`chat-completions` 对应 `openai-completions`，`responses` 对应 `openai-responses`，`messages` 对应 `anthropic-messages`，`generate-content` 对应 `google-generative-ai`。预设供应商的 `baseUrl` 留空，运行时按模型自己的 `baseUrl` 取地址。

1. 目录同步：`node scripts/sync-opencode-go-catalog.mjs` 只改 `opencode-go`。确认条目包含 `deepseek-v4.1-flash`、`gpt-6-luna`、`claude-haiku-5-5`、`gemini-4-argon`，且 `baseUrl` 是 `https://opencode.ai/inference/go/...`。重新运行 `scripts/import-provider-catalog.mjs` 会覆盖这个条目，之后必须再跑同步脚本。
2. 首次启动选择 `OpenCode Go`：输入密钥后列出目录里的全部模型（带协议，数量随上游目录变化），用方向键或输入关键字选择一个；随后显示 `Checking the connection with <所选模型>…`，成功时显示 `Connected. OpenCode Go is saved as the default provider.`。`/model` 里应能看到全部模型，四种协议的模型都在。
3. 真实对话：按 chat、responses、messages、Gemini 四种协议各问一句 `say hi`，都要有回答。可用 `deepseek-v4.1-flash`、`grok-4.7`、`minimax-m3`、`gemini-4-argon`。`gpt-6-luna`、`gpt-5.6-luna` 和 `claude-haiku-5-5` 在部分网络或地区会被上游拒绝（`unsupported_country_region_territory` 或 `Endpoint is unavailable`），这属于上游可用性，不是本地协议错误；先用上面可用的模型确认链路。
4. `/provider add` 输入 `https://opencode.ai/v1`：应提示 `OpenCode Go serves this protocol at https://opencode.ai/inference/go/openai/v1; using that URL.` 和 `For every OpenCode model and protocol, choose the OpenCode Go preset (/provider add opencode-go).`，密钥默认值变为 `OPENCODE_API_KEY`，并列出模型；保存后显示 `Connected with ...`。
5. 错误地址：`/provider add` 输入 `https://example.com/v1` 这类返回网页的地址，应看到 `The endpoint answered 404 Not Found with a web page, not an API response; check that the Base URL is the API base (it usually ends in /v1).` 和 `Check the Base URL and key, or /cancel and run /provider add again.`；保存后连接测试失败仍保留供应商，并提示 `... is saved; fix it with /provider edit ...`。
6. 上游按地区拒绝模型：`/provider add` 列出模型后直接回车用第 1 项（`beam-free`），连接测试应显示 `service reported an error: model unavailable in this region (regionerror)`，再补一行 `beam-free is not available in this region; pick another model with /provider edit ...`，而不是 `no recognized error classification`。用 `/provider edit` 换成 `deepseek-v4.1-flash` 应显示 `Connected with deepseek-v4.1-flash.`。
7. `/provider add opencode-go` 直接进入预设流程，不经过通用向导；同样先列出全部模型，选择后只测试所选模型。
8. 会话头：只有 OpenCode 的请求带 `x-opencode-session`（内置 `opencode`、`opencode-go` 或 Base URL 主机是 `opencode.ai`），值是会话 ID；连接测试等会话外请求回退为请求 ID，保证这个头始终存在。缺少这个头时上游返回 `400 MissingSessionID`，可作为反向验证。其他供应商不再收到 `x-session-id`、`session-id`、`thread-id`，用本地 mock 端点核对实际请求头。
9. 连接测试用本地 SSE fixture 返回 `usage: null` 或省略 `usage`：应正常保留回复与结束原因，不提示 `expected struct OpenAIUsage`；空用量前后夹着有效用量时保留统计；非空但类型错误的用量仍应报错。同时核对会话外请求的 `x-opencode-session` 等于 `x-request-id`。自动用例：`cargo test --locked -p deepseek-code-core --lib opencode_connection_test_accepts_optional_usage_and_rejects_invalid_usage`（一个测试覆盖五种用量序列）。

自动回归：`cargo test --locked -p deepseek-code-cli opencode`、`cargo test --locked -p deepseek-code-cli presets_use_catalog_models`、`cargo test --locked -p deepseek-code-cli wizard_keeps_a_provider_saved_when_the_connection_test_fails`、`cargo test --locked -p deepseek-code-core model_listing`、`cargo test --locked -p deepseek-code-core request_identity`、`cargo test --locked -p deepseek-code-core provider_runtime`、`cargo test --locked -p deepseek-code-core --lib performance::tests::region_and_session`、`npm run test:run -- modelCapabilities`。单元测试覆盖 URL 纠正、预设字段、目录 baseUrl 推导、网页错误、只发 OpenCode 的会话头与会话外回退、非 OpenCode 不发送会话头，以及地区拒绝时只输出固定词表（不回显上游正文）；真实终端步骤仍要按第 2 到第 7 条执行。

### J. 欢迎区与计划模式提示

1. 新会话分别发送普通问题、`/plan <任务>`，以及先 `/plan` 再提问：欢迎区三行保持启动位置，计划模式提示使用 `DeepSeek Code`。
2. 本地模拟接口分别保持流式输出、返回短回答、断开连接：运行中、完成和连接失败后欢迎区不下移，输入框保持底部；第二轮短对话同样检查。
3. 运行中和等待输入时缩放窗口，再切换 `Alt+M` Compact 并返回普通视图：仍可容纳欢迎区时三行保持原位。长输出将欢迎区正常滚出后，最新问题固定栏出现，放大窗口不复活旧欢迎区。
4. 恢复仅含文本回答的会话：最新问题固定栏正常显示，`Ctrl+Home` 能到最早的问题；再检查排队撤回、工具展开和 `Esc` 中断。
5. 在独立数据目录和工作目录中运行 `welcome-plan-acceptance-20261106.mjs`，传入本次构建产物；脚本与真实 ConPTY 文本、原始终端输出及断言结果位于相邻开发支持目录。不得用正在运行的已安装程序替代本次构建产物。

### K. 模型请求失败的诊断与重试

全部使用本机可控的 HTTP/TCP 故障服务，不使用真实密钥或计费接口。自动化分三层：

- `cargo test --locked -p deepseek-code-core --test request_failure`：故障注入、分类、预算、取消与脱敏。
- `cargo test --locked -p deepseek-code-core --test api_failure_audit_probe`：审计发现的三个泄漏输入（多项 Cookie、Basic 代理凭据、服务端回显请求正文）。
- `cargo test --locked -p deepseek-code-core --test api_failure_reaudit_probe`：二次审计的输入（带空白的 JSON 键、短纯文本回显、多种嵌套与转义形式），以及“可识别类别仍保留”的反向用例。
- `cargo test --locked -p deepseek-code-core --test api_failure_review3_probe`：三次审计的输入（`message`、`detail`、`error_description`、`code` 四个字段中的纯自由文本）。
- `cargo test --locked -p deepseek-code-cli --test api_failure_diagnostics`：真实 CLI 进程对本地故障服务，核对最终输出、服务端实际请求数与 SQLite 中的请求快照。
- `cargo test --locked -p deepseek-code-cli --test background_failure_diagnostics`：真实 supervisor 启动真实 CLI Worker，核对服务端请求数、任务输出（`/tasks show` 展示的内容）与会话快照三者一致；同一目标覆盖后台任务的认证错误与取消。

以下终端步骤需要在真实 ConPTY 中手工执行，使用本次构建的 `dcl.exe`。原有的 `terminal_acceptance.py` 自动化脚本已在 2026-11-10 的开发支持目录清理中随证据删除，步骤本身仍然适用。

1. 首次连接读走请求后直接关闭，第二次正常返回 SSE：请求成功，回答只出现一次，实际请求 2 次。
2. 每次都在发送阶段断开：达到“1 加 HTTP 重试配置”的上限后结束；最终输出、请求快照与任务记录中的尝试次数一致，重试次数等于尝试次数减一。
3. 服务端返回 503 并带 `Retry-After`：退避期间按 `Esc` 中断，取消后不再出现新的请求记录。
4. 服务端保持连接但不返回响应：在等待响应期间按 `Esc`，取消后超过下一次重试时刻仍没有新请求。
5. 已输出部分正文后断开：已显示内容只出现一次，不自动重放，摘要标明已开始输出；已完成的工具不再执行，历史不重复写入。
6. 401 与 400 参数错误：只请求一次，摘要分别显示认证失败与“参数错误”类别；服务端自由描述文字不再出现，已知错误标识（如 `invalid_request_error`）会保留。
7. 脱敏输入：`Cookie` 的每一项、`Authorization` 与 `Proxy-Authorization` 的完整值（含 `Basic` 编码凭据）、URL 查询参数，以及服务端回显的请求正文，都不出现在摘要、日志、事件、请求快照或会话记录中。服务端把用户提问回显到 `error.message` 时同样不出现。
8. 底层错误经过多层包装后仍能显示阶段、类别和次数；三处（最终输出、请求快照、任务记录）的数字一致。
9. 流式响应在收到部分内容后正常结束但缺少完成标记：阶段为流式中断、HTTP 状态为已收到的 200、尝试次数为发送层真实值。OpenAI 与 Anthropic 两条协议各跑一次；内置适配器与 Responses 协议不经过共享发送层，次数与状态保持 `unknown`，不能用推测值替代。

同一目录的 `desktop/fixture.py` 配合真实 Desktop 窗口核对错误卡片上的阶段、次数与脱敏结果。

### L. 超长历史的摘要分段、每段恢复与分层合并

全部使用本地模拟接口和临时数据库，不发真实请求。

1. `cargo test --locked -p deepseek-code-core --test compaction_status_regressions`（3 项）：指令开销的 `unknown_parts` 不为 0 时估算不得标为完整；与压缩边界同一毫秒、但没有记录该边界的旧请求不得成为当前用量；旧分支复制了祖先边界且没有计数键时，次数为“未知（旧历史）”、可证明的自身次数为 0，不显示精确的祖先累计值。
2. `cargo test --locked -p deepseek-code-core --test compaction_boundary_provenance`（6 项，均覆盖 OpenAI 与 Anthropic）：先按当前历史读取消息与压缩边界，再由另一入口提交新的压缩，随后用旧消息发出真实 HTTP 请求。断言服务端正文是旧上下文、不含新摘要，且每个请求快照记录的 `contextBoundary` 都不是新边界；没有可信来源的主机消息记录为未知边界。把执行器改回“发送时查询数据库最新边界”后这 4 项全部失败，证明它们确实覆盖该缺陷。
3. `cargo test --locked -p deepseek-code-core --test compaction_truncation`（37 项）：两个协议下分段与最终摘要提额且篇幅目标不变、模型上限下按 token 二分并合并、合并再次截断后提额、合并持续截断停在深度上限并复用已完成分段、单段额度耗尽时其它分段继续完成、恢复合并超过 64K 时拒绝发送、恢复结果在失败和重启后复用、合并超出安全输入预算、最后实际预算与拟请求预算分开报告、窗口过小与分段规划失败的完整诊断、非截断立即停止、截断文本不进入缓存、缓存写入失败不提交、并发修改历史拒绝提交、局部摘要截断、约 85 万 tokens 历史的分段与分层合并、约 80 万 tokens 的单段失败隔离与缓存复用、长摘要形成小于 8 份的分组并单项传递、空正文按原请求重试一次后安装、空正文重试仍为空时进入拆分、超长正文直接拆分且不提额也不原样重发、截断后提额返回空正文时先在提额后的预算原样重试、重试成功时不再拆分、重试仍为空时按原分段预算拆分、截断后提额返回超长正文时按原预算拆分且不再提额、持续空正文停在深度上限且不写缓存、持续空正文与持续超长正文的最终错误保留结束原因、字符数、预算与用量、空正文导致的分段失败不影响其它分段并在重试时复用它们、大量分段共用额度时拆分请求未发出仍报告触发它的超长正文、空正文重试后再拆分同样保留诊断、取消后新增真实记录时内容未变的分段不再重发（同进程）、同一场景在关闭并重新打开数据库与引擎后仍只重发改变的分段。多数用例在 OpenAI 与 Anthropic 各跑一遍；`persistent_empty_summaries_stop_at_the_depth_limit_and_cache_nothing` 只跑 OpenAI。
4. `cargo test --locked -p deepseek-code-core --test compaction_overflow`：原有分段、取消、失败与最终摘要预算不变；合并首次用 4096、截断后按模型配置提高到 8192。
5. OpenAI 的 `length` 与 Anthropic 的 `max_tokens` 都按结构化结束原因识别，不依赖错误文字；其他结束原因仍按普通失败处理，不进入拆分，且不新增请求。正常结束但正文为空或超过 32,000 个字符同样是结构化结果，不当作输出上限提额，也不会保存或替换上下文。
6. 单次摘要请求的完整输入（系统指令、阶段提示词和历史）不超过 64,000 tokens，即使模型窗口能装下整段历史也先分段。该上限覆盖恢复合并：两份子摘要拼起来超过 64K 时，即使 1M 窗口仍装得下，也拒绝发送并只让该分段失败。约 85 万 tokens 的合成历史产生 14 个左右的分段，每个请求都在上限内，随后经 3 次分层合并得到最终摘要。
7. 提额只增加输出预算，不改变篇幅目标：分段与合并始终约为 2048 tokens，最终摘要始终约为 4096 tokens。测试直接断言请求体中的目标文字。模型上限为 4096 时跳过提额，拆成两段后合并，内容顺序、中文、emoji 与超长工具结果完整。
8. 每个初始分段有独立恢复额度（通常 8 次），成功分段的剩余额度转入合并池；合并池初始 16 次，每个中间合并或最终摘要节点最多使用 8 次。某段额度、拆分深度或输入预算耗尽时，错误列出失败分段的数量与索引、该段拟请求与最后实际发送的预算、已用次数与原因；其中因“无法发出下一个请求”而结束的恢复，还会保留触发它的那次响应（分段、结束原因、字符数与字符上限、实际预算、已报告或未知用量），不会退化成只说明额度用尽。其它分段继续完成并写入缓存，`/history` 中的原始记录不变，缓存中没有截断文本。
9. 合并按时间顺序分层，每组最多 8 份摘要且不超过单次输入上限；单剩余项直接进入下一层；摘要数量无法减少、或连两份摘要都放不进合并输入预算时明确失败，不发送超限请求、不循环。
10. 恢复过程中关闭并重新打开数据库与引擎后重试，或中途取消后重试，已完成的分段与子摘要都直接复用；恢复期间历史发生变化时拒绝提交，原有记录与新增记录都保留。
11. 手动 `/compact` 与自动压缩显示同一套逐段进度。
12. 真实 ConPTY：相邻开发支持目录的 `compaction-recovery-20261107/compaction_truncation_acceptance.py` 用隔离数据目录和本地模拟接口。每次运行写入独立目录（`--out`，或 `runs/<时间>-<随机>`），目录内保存 `run.json`（二进制路径、SHA256、运行时源码 revision 与是否有未提交改动、相关源码与验收脚本的 SHA256、场景参数、起止时间）和 `results.json`；非空目录直接拒绝，不会覆盖旧证据。运行时 revision 只说明当前检出，不能当作二进制的构建来源。每个场景声明必须产生的检查项；缺少必需项、任一项为假或场景报错都判为失败，重启未就绪与取消被忽略都不能跳过。场景：
    - `auto`、`manual`（窗口 14,000）：超大历史、两段摘要、首段截断。断言原始分段恰为 2、恢复子段恰为 2、合并在子段之后，核对终端 `fragment 1/2` 与 `fragment 2/2`；自动场景还检查 `Auto-compacting conversation`、压缩后原请求继续且只执行一次并携带摘要，手动场景检查 `/compact` 的进度与 `Compacted in … (ctrl+o to see full summary)`。
    - `large`、`large-manual`（窗口 100,000）：历史约 86k tokens，整段能放进 90% 安全线但超过 64K 单次上限，因此仍拆成两段；其中 64K 分段连续两次触顶（首请求与提额后），拆分出的 32K 子段正常完成。断言所有摘要请求不超过 64K、至少两次截断被恢复、进度可见；自动场景还验证原请求只继续一次并携带摘要。
    - `empty-auto`（窗口 100,000）与 `raise-empty-manual`（窗口 14,000）：首段 4096 截断、8192 提额返回空正文，随后在 8192 原样重试成功。断言三次预算依次为 4096/8192/8192、后两次请求体完全相同、该分段之后的所有请求都重复同一输入（没有拆分）；自动场景还要求原请求只继续一次并携带摘要。
    - `raise-oversized-manual`（窗口 14,000）：首段截断、提额返回 32,001 字符。断言第三个请求回到 4096、后续不再提额，两个拆分输入与第一个请求不同，压缩完成并替换历史。
    - `empty-manual`、`blank-manual`（窗口 14,000）：普通入口直接返回空正文或纯空白正文，断言第二次请求与第一次同预算、同请求体，成功后替换历史，`/compact` 显示完成提示。
    - `oversized-manual`（窗口 14,000）与 `oversized-auto`（窗口 100,000）：32,001 字符正文。断言没有原样重试、没有提额、后续请求是更小的拆分输入，压缩完成并替换历史；自动场景还要求原请求只继续一次。
    - `invalid-restart-manual`（窗口 14,000）：首个分段的请求体带唯一标记，模拟持续返回 32,001 字符；另一个分段正常完成。断言失败信息同时包含 32,000 字符上限与历史保留、历史不变、缓存中没有超长文本；用 `--continue` 重启后再次 `/compact`，必须就绪、确实发出请求、只请求带标记的分段、已完成的其它分段直接复用，并且再次失败、历史不变。
    - `cancel-auto`（窗口 14,000）：自动压缩的首个分段请求一直不返回，在等待期间按 `Esc`。断言请求确实处于挂起、释放原响应后既没有新请求也没有提交、原有历史作为前缀保持不变、缓存中没有无效正文。
    - `cancel-manual`、`cancel-empty-manual`、`cancel-invalid-manual`（窗口 14,000）：同样的挂起分别发生在手动 `/compact` 的首个分段、空正文的同预算重试、以及最后一个分段的拆分恢复。断言 `Esc` 经现有输入读取器取消这次手动压缩，界面显示 `Compaction interrupted before it was saved`，释放迟到响应后没有新请求或提交，已有有效缓存仍在。
    - 四个取消场景随后都用唯一标记发出一次新的普通请求，必须收到与该标记匹配的最终回复、数据库保存该回复、输入恢复后才视为通过；仅“服务器收到请求”不通过。
    - `export-manual`（窗口 14,000，后置钩子延迟 4 秒）：手动压缩成功后读回 `compactions/<会话 ID>/` 下的 Markdown 文件，按固定格式解析会话 ID、带时区时间、消息数和正文，与配置的窗口、独立的活跃消息数、已提交摘要逐项相等比较；正文多一段、少一段、消息数、会话 ID 或缺时区都不通过。显示耗时必须覆盖实际等待（含后置钩子），允许调度误差。`status-manual` 还在压缩后立即 `/status` 检查 `estimated after compact`、解析出的窗口数值等于配置值、`Compactions: 1`，再用受控用量发出一次新请求检查切回 `last request`，最后重启确认次数与实测用量都保留。
    - `post-commit-manual`（窗口 14,000，后置钩子延迟 8 秒）：等边界和摘要文件都出现后再按 `Esc`。断言界面显示 `Compacted in …`、`Stopped the remaining post-compaction steps` 和提交后真实等待的耗时（与实际区间一致），不出现 `before it was saved`；边界只增加一条、文件只有一个、次数只加一，路径仍可见。随后的唯一标记请求复用提交前取消场景的完整判定：匹配回复可见、写入数据库、输入恢复，缺一不算通过。
    - `cancel-ctrl-c-manual`（窗口 14,000）：与 `cancel-manual` 相同的挂起，用 `Ctrl+C` 而不是 `Esc` 取消，其余判定相同。
    - `cancel-merge-manual`（窗口 14,000）：分段正常完成、挂起合并请求时按 `Esc`。断言合并阶段同样被取消，释放迟到响应后没有新请求或提交，已有分段摘要仍然有效。
    - `export-auto`（窗口 14,000）：自动压缩完成后读回摘要文件，断言自动入口同样显示耗时与路径、文件元数据与正文正确。
    - `export-failed-manual`（窗口 14,000）：把导出目录占位成普通文件，使导出必然失败。断压缩照常提交、`Summary export failed` 可见、没有摘要文件、次数只加一，随后唯一标记的普通请求完成并被保存。
    - 计时口径：脚本用独立 PreCompact 标记观察钩子开始，以本次命令之后的完成提示为终点，使用单调时钟；明确记录这是不含更早规划的阶段区间。输入等待单独记录，首个摘要 HTTP 请求和压缩后的主请求都不能充当操作起点。8 秒前/后置钩子另有独立下限检查，故意漏算该阶段的时长必须失败。延迟 Enter 的受控探针要求启动等待被单独记录且耗时判定通过。
    `safe_budget_respected` 使用与引擎相同口径的完整请求体估算（含系统指令、按消息分组向上取整），只检查摘要路径的请求。所有场景的日志记录每个请求实际收到的注入类型与请求体哈希，不根据预算猜测。`compaction-review2-20261108/harness_negative_probe.py` 与 `harness_negative_probe2.py` 用受控假对象回放“重启未就绪”“忽略 Escape”“缺少必需检查”“存在假检查”“没有必需项”和“场景报错”，要求判定器一律返回非零；它们不替代真实重启或真实复用的场景。`compaction-review3-20261108/negative_probe_post_cancel.py` 让后续请求到达服务器但永不返回，要求汇总器因缺少完整回复、持久化与输入恢复而失败，同时证明请求确实已到达。`compaction-review4-20261108/harness_semantic_probe2.py` 把错误窗口、错误消息数、错误会话 ID、缺时区、额外正文、缺正文、错误耗时和丢失空格的路径逐个注入同一组检查函数，要求全部被拒绝，并用正确样本确认仍然通过。

### 第十一轮：SDK 回合内引导（mid-turn steering）

stream-json 宿主在一轮进行中发送的普通用户消息不再只等这一轮结束，而是通过 `services/input_queue` 的 Steer 模式在下一个工具边界交给正在运行的回合；REPL 与 Desktop 的既有路径不变。全部使用本地模拟模型与临时数据目录。

- `cargo test --locked -p deepseek-code-cli --test sdk steering::`：6 项。工具运行中发送第二条消息后，第二条消息出现在工具结果之后的第二个模型请求里，回合继续；两条消息按到达顺序各出现一次；模型直接给出最终回答时该消息作为下一轮恰好运行一次；`/status` 等斜杠命令不进入运行中的回合；`interrupt` 保留尚未交付的消息并在下一轮运行；`--replay-user-messages` 在交付时以原 `uuid` 回显用户信封，且交付事件、持久化消息各一次。
- `cargo test --locked -p deepseek-code-core --lib services::input_queue`：包含 `undelivered_steer_items_are_cancelled_and_delivered_ones_stay`，确认未交付项被取消并返回、已 handoff 项保留。
- `cargo test --locked -p deepseek-code-core --lib durable_steer_handoff_waits_for_boundary_and_pause_preserves_next_turn`：原有边界与暂停语义不变。
- `cargo test --locked -p deepseek-code-cli --test sdk`：包含 `controls::running_turn_answers_queries_and_cancels_only_queued_messages`（运行中发消息后 `cancel_async_message` 取消尚未开始的引导消息）与 `hook_callbacks::sdk_eof_during_model_or_callback_cannot_continue_automatically_allowed_write`（宿主断开且仍需 Hook/MCP 回调时，排队消息连同引导消息一起丢弃，只发一个错误 `result`）。
- 真实模型（本地不接受真实密钥的步骤不在此列）：用 `--input-format stream-json` 保持 stdin 打开，先发送要求三次 `sleep` 工具调用的消息，8 秒后发送“跳过剩余 sleep”的消息。预期 `queued_input` 事件在第一次工具结果之后出现，第二个模型请求包含该文本，剩余调用可以不再执行。2026-11-10 的基线与本地模拟证据在相邻开发支持目录 `steer-20261110/`。

事件形状（与 REPL/Desktop 共用的 `queued_input` 行）：`{"type":"queued_input","text":"…","content":"…","origin":"queue-input-<id>"}`。`origin` 是持久队列项的 handoff 标识；引导消息自己的 `messages` 行已由 handoff 保存，故不再作为工具附件重复写入。REPL/Desktop 的 legacy 队列项保持原有 `origin`（`user`、`task_notification`）并继续依附最近工具结果。

## 按改动范围选择场景

- `terminal/editor.rs`、`terminal/live.rs` 的按键处理：A、B、E1、E2、E3
- `terminal/live.rs`、`terminal/scrollback.rs`、`terminal/change_view.rs`、`terminal/tool_view.rs` 的显示：B、C、E1
- `repl.rs`：C4、D1、D2、D6、D7，以及 L（手动 `/compact` 的完成输出、提交前取消与 `post-commit-manual` 的提交后取消）
- `terminal/input.rs` 的取消通道：L 的 `cancel-manual`、`cancel-empty-manual`、`cancel-invalid-manual`、`cancel-merge-manual`、`post-commit-manual`，并在真实终端确认 `Esc` 不滞留到下一个提示、等待期间 `Ctrl+O` 仍能打开完整记录
- `status_commands.rs`、`services/context_usage.rs`：`status_commands_tests`、`compaction_status_regressions` 与 L 的 `status-manual`（按边界与配置匹配用量、部分估算、压缩后估算、次数持久化）
- `session_commands.rs`：L（自动压缩完成输出与次数）
- `worker_frontend.rs`、`background_commands.rs`：B、D、E3、E4、F
- `memory.rs`、`display_commands.rs`、`crates/core/src/memory`：G
- `services/customization.rs`、`prompt/`、`native_engine/tool_loop.rs`：H，以及 SDK 测试
- `tools/cron_jobs.rs`、`tools/wakeup.rs`、`main.rs` 的空闲循环：I
- `terminal/picker.rs`：F，以及 `/resume`、`/rewind`、`/mcp` 等使用选择列表的命令
- 欢迎区生命周期、输入框与运行帧交接、`/plan` 提示：J，以及 B、C、E1、E3
- `session.rs`：B、D、E；SDK 输入路径另见“第十一轮：SDK 回合内引导”
- `services/session_worker*`、`services/background_sessions.rs`：D、E4，以及 `cargo test --locked -p deepseek-code-core --test worker_environment`（普通子进程不继承 worker 的控制变量）
- `tools/mod.rs` 的 shell 启动、`services/tool_hooks.rs`、`mcp/mod.rs` 的 stdio 启动：`worker_environment`，并按 E4 复核后台与分离命令
- `runtime.rs`、`db/message_repo.rs`（发给模型的历史）：D2、E5，以及 SDK 测试
- `db/mod.rs`（`write_tx`）：`cargo test --locked -p deepseek-code-core --lib db::write_tx`（重试、非争用不重试、超时报 label、失败不留半行）与 `cargo test --locked -p deepseek-code-core --test session_concurrency`。真实争用复验：用 `VACUUM INTO` 复制数据库，`DEEPSEEK_CODE_DATA_DIR` 指向副本，另一个进程 `BEGIN IMMEDIATE` 占锁 8 秒，期间跑一轮带工具调用的请求，确认回合完成、日志有重试告警、工具行与快照完整；再占锁 40 秒，确认回合以带 label 的错误结束（不是裸 `database is locked`），且没有半成品工具行。
- `db/message_repo.rs` 的请求快照去重（`request_blobs` / `*Ref`）：`cargo test --locked -p deepseek-code-core --lib db::message_repo`（同头只存一份、往返字段一致、旧内联行可读、缺 blob 不伪造、不修改调用方数据）、`cargo test --locked -p deepseek-code-core --test compaction_status_regressions` 与 `cargo test --locked -p deepseek-code-cli --bin dcl status_commands_tests`。历史数据不会自动回填：要缩小现有库需另跑一次性回填脚本并 `VACUUM`，验证方法是回填前后比较 `requests_json` 总字节、`request_blobs` 行数，并抽 3 条消息确认 `get_request_snapshots` 往返一致。
- `services/input_queue.rs`：B、E4，以及“第十一轮：SDK 回合内引导”的全部自动用例
- `sdk.rs`（SDK stream-json 输入与回合控制）：E6、SDK 全套，以及“第十一轮：SDK 回合内引导”
- 权限相关：E2，以及 `added_directory_survives_worker_resume_without_granting_other_sessions`（`bypass` 模式可以读工作区外的文件，其他模式受目录范围限制）
- Provider、模型请求：E1、E6、M（OpenCode Go 的预设、URL 纠正、失败提示与会话头）
- `scripts/sync-opencode-go-catalog.mjs`、`src/data/providerCatalog.json`：M，以及 `npm run test:run -- modelCapabilities`
- `interactive_config.rs`：M 的预设、URL 纠正、保存后连接测试和失败提示
- `native_engine/model_listing.rs`：M 的网页响应提示
- `native_engine/request_identity.rs`：M 的会话头
- `native_engine/provider_runtime.rs`：M 的目录 baseUrl 推导和四种协议真实对话
- `native_engine/stream_retry.rs`、`native_engine/utf8_stream.rs`、`native_engine/request_trace.rs`、`performance/`：K，以及 E1
- `project_instructions.rs`、`services/lifecycle_events.rs`：`cargo test --locked -p deepseek-code-core --lib project_instructions`，并确认用户级说明只来自数据目录、项目说明只取用户主目录以下的上级目录
- `native_engine/tool_loop.rs` 的错误与请求快照路径：K、H
- `native_engine/generation.rs`、`services/compaction.rs`、`services/compaction/`：L，以及 E5
- `native_engine/tool_call_ids.rs`（发给模型的请求体）：E1、E6；确认只改写超长调用 ID，未提供 `tool_calls`、`content` 等字段的消息不会被补成 `null`
- `extended_commands.rs` 的复制（`/copy`、`/copy-session-id`、鼠标选择）：在没有 `xclip`、`wl-copy` 的 tmux 中执行 `/copy-session-id`，显示 `Copied session ID to clipboard`，`tmux show-buffer` 内容为会话 ID；在有 `xclip` 的桌面终端中复制后能粘贴。再跑 E
- 其他范围：至少跑 E

## Shell 配置回归

- 使用独立数据目录验证 `config set shell.*`、`config show`、`!` 和模型工具；环境变量优先于文件，显式工具 Shell 优先于默认值，无效配置不能静默回退。
- Windows Git Bash、CMD、PowerShell 验证非零退出、语法错误、中文与空格路径、跨调用工作目录。真实 WSL 验证命令超时和后台取消后 Linux 子进程不再运行。
- 管道退出码：`false | head -1` 在 bash 和 Git Bash 下必须报失败；`printf 'a\nb\n' | head -1` 报成功且输出为 `a`。`pipeline_sigpipe` 只在 bash 系 shell 的 141 上出现，PowerShell 和 CMD 不认这个数字。
- 进程树：普通调用结束后其子进程不得残留（超时、取消、正常退出三种情况各验一次）。`detach: true` 返回 `pid` 与日志文件；`isolated` 为 `false` 时提示必须说明进程可能随调用结束，不能声称一定存活。与 `run_in_background` 或 `timeout` 同用时明确报错。
- `cargo test --locked -p deepseek-code-core --lib wsl -- --include-ignored --test-threads=1` 使用明确选择的 `dcl-linux-test` 发行版，不依赖系统默认发行版。

## 尚未验证

- 本次 Windows ConPTY 已验证下列审批、工具展示和数据库争用场景；不代表这份清单的所有场景都已完成。macOS 尚未验证。`Ctrl+J`、鼠标点击和进程退出判断都和平台有关，改动相关逻辑后仍要在对应终端确认。
- OpenCode Go 的真实接入已在 Linux 验证。Windows、macOS，以及上文中被上游按地区拒绝的模型尚未验证。

## Shell 审批与工具展示回归

1. Auto 模式执行普通 Bash 和复合读取命令时不弹审批，也不请求分类器；PreToolUse 修改命令后，以最终输入执行并传给 PostToolUse，工具调用 ID 保持一致。
2. Ask 模式为普通 Bash 选择 `Allow for Session`，同一会话下一条普通 Bash 直接执行；危险规则命中的命令仍单独审批，菜单不提供会话授权。
3. `/permissions bash-session show` 显示已授权，`revoke` 后显示未授权；Worker 重连保留授权，冷启动、其他会话不继承授权。显式拒绝规则保持优先。
4. `git_add` 请求 12 个路径时，折叠结果显示成功摘要、3 个路径和 `+9 lines (click to expand)`；实际点击后能看到第 12 个路径，复制内容为完整文本。
5. 后台任务运行时，`TaskOutput(任务名称)` 在等待结果前出现；恢复历史后仍显示名称。
6. 工具运行期间用独立 SQLite 连接持有写事务 3 秒：前台不弹锁库提示，继续显示工具结果并保留已有建议；持续锁超过 5 秒才提示状态延迟。释放锁后正常结束，过期建议不能获批；任务目录中没有新增 `cancel` 文件。
7. 空标准输入失败后允许保留诊断日志，但不能创建会话数据库或配置；损坏的配置文件不能被覆盖。
8. Windows 短暂独占 `pending-question.json` 后释放，前台仍能读取审批；持续占用时在有限重试后报告具体文件路径。文件已删除则视为审批已处理，其他会话不能读取或回答该审批。

## Plan 联网与文件编辑授权回归

1. Plan 调用新的公网域名 Fetch，无域名审批且状态保持 Plan；本机和内网地址每次询问，批准后真实访问，拒绝不发请求。覆盖 Auto、Bypass、无宿主、跨主机重定向和不支持的协议。
2. Auto 创建、编辑普通文件（包括工作区外确切文件），不调用分类器或请求确认；受保护路径、Hook、显式规则仍有效。
3. Ask 编辑菜单选择 `Allow for this session`：本轮及下一轮跨文件编辑直接执行，状态仍是 Ask；菜单中的 accept-edits 切换选项仍可独立选择。
4. `/permissions edit-session show`、Allow 页显示授权；撤销后再次编辑需确认。Worker 重连保留授权，冷启动、复制和子会话不继承。
5. 无效参数、拒绝、取消、过期、重复审批均不得留下会话编辑授权；切入 Plan 后编辑普通文件仍拒绝。

### 重连等待与逐次诊断

- `cargo test --locked -p deepseek-code-core --test reconnect_policy`：虚拟时钟验证 8 次尝试、2 至 128 秒七次退避、Retry-After 300 秒上限、取消与逐次脱敏。
- `cargo test --locked -p deepseek-code-cli --test api_failure_diagnostics --test background_failure_diagnostics`：本地服务和独立目录，检查 8 次上限、重连提示、独立日志以及 Worker 状态。用例仅在隔离配置中缩短基础等待，生产配置仍为 2000 毫秒。
- 真实 ConPTY：前两次失败后成功，确认“正在重连”“第2次重连”和等待秒数；Retry-After 等待时 Esc 取消后无新增请求。检查 `logs/request-attempts/<session-id>.jsonl` 中逐次状态及本轮结束状态，不保存请求正文。

### 第六轮新增复核入口

- `cargo test --locked -p deepseek-code-cli --bin dcl compaction_cancellation_tests`：自动取消的旧前缀和统一 Interrupted 错误归为取消，普通模型/Hook 失败不误判。
- `cargo test --locked -p deepseek-code-core --test compaction_truncation invalid_recovery_cancel_grow_restart`：空正文、超长正文恢复中，已有两个有效子摘要后取消，新增真实历史并重开引擎；OpenAI/Anthropic 均检查相同请求不重发、改变的尾部重规划。
- `compaction-review6-20261109/auto_cancel_verify.py 3|6 [ctrl-c]`：独立 ConPTY 回读完成路径/耗时、边界/文件/次数、worker 和 execution_turns 取消状态，以及下一唯一标记回复的显示、保存和输入恢复。
- `timing_verify.py auto|pre`：8 秒 PostCompact / 8 秒 PreCompact 加 1 秒 PostCompact；正例通过且零时长负例必须失败。
- `browse_resize_verify.py`：80 列进入 Ctrl+O，完成时仍浏览，resize 到 120 列后返回，核对路径与单次提交及下一请求。
- `status_pending_verify.py`：自动压缩后挂起主请求，检查 Status 仍为 estimated after compact 和 Compactions: 1。
- 上述脚本与分次证据已在 2026-11-10 的开发支持目录清理中删除，清单保留为可重跑步骤；未执行组合不视为通过。

### 第七轮：超限恢复与轮次生命周期

- `cargo test --locked -p deepseek-code-core --lib db::turn_repo`：只结束一次、非正常stop reason、ContextReset分段共用turn，以及删除失败segment后clear_segment为同一turn重新关联。
- `cargo test --locked -p deepseek-code-cli --test sdk --test agent_hooks --test async_hooks --test async_rewake`：SDK与初始hook/hook wake入口回归；运行中turn_timing可以更新，最终非running计时、result与request_turn_end每轮各一次。
- `compaction-review7-20261109/recovery_outcomes.py success|summary-failed|retry-failed`：独立本地HTTP服务，128K窗口，第二个主请求明确超限；检查同一turn覆盖首次请求、恢复与重试，最终Completed/Failed、SDK只一次终结，尝试诊断及model_requests保留、所有轮次都有有效assistant关联。
- `run_cancel_matrix.py`：真实ConPTY，reactive摘要提交前Esc、提交后Esc/Ctrl+C；worker和execution_turns均Cancelled；保存结果按阶段显示，取消后不发送第三个主请求，下一唯一请求完成并保存；没有重复边界/文件/次数，也没有悬空message_turns。
- 第七轮的产品构建、逐项结果与证据记录已在 2026-11-10 的开发支持目录清理中删除，仅保留上述步骤。不用更改已结束轮次或删除finish_at的单次结束保护来修复恢复取消。

### 第八轮：恢复结果契约与诊断日志故障

- `cargo test --locked -p deepseek-code-cli --bin dcl recovery_result_tests`：跨attempt汇总已报告用量、轮数、拒绝，未知不写成精确0，会话费用不相加；无attempt数据也保留完整字段。
- `cargo test --locked -p deepseek-code-cli --test recovery_result_contract`：本机HTTP与隔离数据库，六种组合（无历史、成功、摘要失败、重试失败、日志目录冲突后恢复、日志冲突叠加摘要失败）。检查完整SDK结构、一次终态、无历史原始context_limit、业务失败不被日志错误替换。
- 固定验收入口 `compaction-review8-20261109/run_acceptance.py <二进制>`：先把自身、fixture和所有本地导入复制到独立runs目录，实际执行副本并记录前后SHA256与真实退出码。OpenAI/Anthropic分别运行恢复及SDK取消契约，OpenAI另跑先发生工具拒绝/已报告用量再超限的场景。
- 原 `compaction_truncation_acceptance.py` CLI入口同样先固定自身再执行副本，snapshot.json记录执行位置及前后哈希；run/results仍写回指定输出目录，详细场景证据位于evidence子目录。新增脚本不会用后来复制的文件追认旧证据。
- 第八轮的执行状态与协议覆盖边界记录已随 2026-11-10 清理删除，不能作为本次通过的依据；`cache_sequence.py` 的检查保留为可重跑步骤：提交前取消保留完整缓存，确认重试无新增摘要HTTP、连续两次各自计时与次数达到2；精确竞态、重启回放、配置切换及CLI重置/计划切换迟到响应等原任务仍需独立场景，不由协议契约测试替代。

### 第九轮：同轮多请求用量

- `cargo test --locked -p deepseek-code-cli --bin dcl request_usage_tests`：按请求ID保留最新非null报告，重复快照不相加；覆盖Anthropic分段、OpenAI终态、未知字段、嵌套缓存与跨分段/恢复汇总。
- `cargo test --locked -p deepseek-code-core --lib native_engine::request_trace::tests`：请求内重复累积字段替换而非相加，后续null不擦除已报告计数或缓存字段。
- `cargo test --locked -p deepseek-code-cli --test usage_accounting`：真实本地HTTP/CLI，OpenAI和Anthropic各三次模型响应合计51输入/10输出，同请求重复累积及最后null更新不重复计数；OpenAI total=61，缓存按各自协议保留。未知usage保持空对象，下一用户轮次只统计自身7/1。
- `cargo test --locked -p deepseek-code-cli --test sdk sdk_plan_clear_boundary_survives_actual_cli_process`：真实计划审批和ContextReset，两协议的第一次10/4和重置后的10/4均在SDK结果中保留为20/8。
- 第九轮脚本（已在 2026-11-10 清理中删除，按本条重建）在固定副本中复跑第八轮20场景，并新增 `multi_round_usage.py` known-stats：压缩前两次工具拒绝报告11/2及17/3，恢复后23/5，断言51/10/61、num_turns=3、两条拒绝及唯一终结。需显式传入本轮新构建。

### 第十轮：SDK interrupt 统一收尾

- `cargo test --locked -p deepseek-code-cli --test sdk interrupt_usage::`：本地真实 HTTP/CLI，OpenAI/Anthropic × 已知/未知四组合。流保持打开，已知用量更新 11/1 → 11/2 → 重复/null 后 interrupt；结果保留 11/2，OpenAI total=13，未知保持空对象；受控 120ms 等待纳入真实耗时，终态计时、result、request_turn_end 各一次，SQLite 状态/消息关联有效，下一轮仅统计 7/1。
- `cargo test --locked -p deepseek-code-cli --test sdk recovery_interrupt::`：同一轮先报告 11/2 和一次工具拒绝再超限，在 PreCompact/PostCompact SDK 回调挂起时 interrupt；两协议提交前不保存摘要、提交后恰好一份摘要/边界/计数，不发送重试主请求，保留统计并允许下一轮。
- SDK 全套覆盖审批、Hook、MCP 等待取消及迟到回复；同时保留 `usage_accounting`、`recovery_result_contract`、`request_usage_tests`、ContextReset、初始 Hook/hook wake 回归。
- 第十轮的独立复现与需求证据已在 2026-11-10 的开发支持目录清理中删除。未执行平台或旧验收组合继续标记未验证。
