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

有少数用例依赖真实的进程启动或固定超时，在机器同时进行编译或其他测试时会偶发超时：`services::hook_progress::tests::real_hook_success_failure_timeout_and_cancel_clear_status`（钩子 2 秒超时）。这类失败应先单独重跑同一用例确认；单独通过且代码未涉及时按负载问题处理，不要为了让它通过而放宽产品超时。2026-11-07 的两次全量运行各出现一次，均在单独重跑时通过。

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

## Worker 生命周期：旧代重建、锁释放与下一轮准入

修改 `crates/core/src/services/session_worker.rs`、`crates/core/src/services/session_worker/recovery.rs`、`crates/cli/src/persistent_worker.rs` 或 `crates/cli/src/persistent_child.rs` 后，执行：

```bash
cargo test --locked -p deepseek-code-core --lib services::session_worker
cargo test --locked -p deepseek-code-cli --bin dcl admission_tests
cargo test --locked -p deepseek-code-cli --test session_worker
```

三处行为必须分别验证，不能互相替代：

1. 旧代在就绪前停止时重建。`ensure_worker` 对最近有心跳的旧代只等待，不直接调用 `wait_ready`；旧代在等待中变成 `stopped` 时返回 `None`，由持有 startup lock 的调用方创建新代，新代仍走原 `wait_ready`，因此真正的启动失败不会被无限重试掩盖。用例 `ensure_worker_replaces_a_predecessor_that_stops_before_ready` 在 startup lock 出现后等待 1 秒再发布 `stopped`，覆盖“旧代仍在启动”分支；这是时间门控的复现，不是对某次内部读取顺序的证明。把该分支改回旧的直接 `wait_ready` 后，该用例必须报 `session host exited before becoming ready`。
2. 恢复锁按所有权生命周期释放。`startup.lock` 与 `repair` 中的代际锁用 `fs::unlock_file` 显式释放，而不是只依赖描述符关闭；`flock` 归属打开文件描述，fork 出的子进程持有同一描述符时，仅关闭不会释放锁。用例 `recovery_releases_the_startup_lock_for_a_retained_descriptor` 与 `a_failed_recovery_releases_the_lifetime_locks_it_took` 保留真实描述符副本，旧语义（只在 `Drop` 关闭）下两者都会因 `WouldBlock` 失败。这不覆盖 SIGKILL 或异常退出的边界，那里仍只有描述符关闭兜底。
3. 完成与下一轮准入交接。`persistent_worker` 只对本宿主已准入且持久记录证明已完成的一轮等待释放事件；真正运行中的一轮立即拒绝，等待超时返回未提交且不排队、不重放。`admission_tests` 共 4 项，其中 2 项使用暂停时钟（旧实现 25 x 10 ms 窗口后仍必须继续等待；超时后不写入、不启动第二轮）；另 2 项是同步状态与身份断言。真实协议路径由 `turns_submitted_right_after_completion_are_admitted` 连续 3 轮覆盖，它不强制收尾窗口，因此不是该竞态的确定性复现。

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
3a. **只有 supervisor 被杀**：worker 主进程约 5 秒内自行退出，之后可以恢复。supervisor 在启动 host 之前就把自己的 PID 写进 worker 记录，并通过内部环境变量把同一个 PID 交给 host，因此即使记录在 host 第一个 tick 之前就损坏，host 仍能识别“owner 已退出”并自行退出；它不会因为记录不可读而永久占用会话。host 读到该变量后立即从自己的环境里移除，不传给子进程。
4. **worker 长时间无响应但进程还在**：`/continue` 不接回，提示用 `/tasks show ID` 查看，并给出任务 ID。
5. **会话被占用时发送普通消息**：报错包含任务 ID 和 `/continue`，消息没有提交。已完成但仍在收尾的回合不算占用：发布完成早于 host 释放准入，所以紧跟完成之后的提交会等待真正的释放事件再准入；超过宽限仍未释放时按“未提交”返回 busy，不排队、不重叠。真正运行的回合（含空闲子代理唤醒）立即拒绝。
6. **没有活动任务**：显示 `Session restored; waiting for input`，`/continue` 正常继续，通知只在继续后才唤醒模型。
7. 恢复后的历史里，没有结果的工具调用显示 `No result recorded`。

启动/恢复锁在逻辑结束时显式解锁（`flock` 属于打开文件描述，fork 出的副本会推迟 close 释放），因此恢复返回后可以立即启动新的 worker。

#### D8. 直连无头回合的实时观察

`--print` 和 `--output-format json` 直接执行时，回合以执行租约标为“运行中”，并按秒发布已生成的公开文本和工具行（每个回合只有消息行和工具行，不写逐 token 文件）。恢复时必须按租约和 `execution_turns` 判断真实状态，不能只查 worker 任务；部分回答不能显示成已完成的 `Previous turn`。

1. **原进程运行中恢复**：显示 `A turn is still running in another process since …; this terminal only observes its published progress and closing it does not stop that turn.`，随后显示已发布的工具行和文本；不得出现 `Previous turn … duration unavailable`。观察只读：不取得执行租约，不提交输入，不回答审批，不执行工具。
2. **观察期间的新进度**：只有观察开始之后发布的内容才算增量。长段落按完整行显示；没有换行且超过约 200 字符的文本先显示一段并以 `…` 结尾，不能无限缓冲。
3. **每段独立游标**：一个回合因恢复或压缩包含多个 assistant 段时，每段各自记住已显示位置；重复轮询不重放任何一段。切换到别的会话（`/resume`）会清空状态，空闲会话不显示上一会话的完成信息。
4. **占用期间输入 `/continue`**：只提示继续观察（`Progress appears here as it is published`），不提交新回合。
5. **观察中退出终端**：在空提示符按 `Ctrl+C` 退出本终端（不是 `Esc`），原进程继续运行；服务端请求数、工具执行次数和数据库中的行都不变。
6. **原进程正常结束**：仍打开的观察端先显示结束前写入的最后文本和工具结果，再显示 `Observed turn finished · Worked for …`；结束后的恢复显示 `Worked for … · done` 和 `Session restored; waiting for input…`。观察本身不打开自动唤醒：排队中的任务通知保持未消费，也不额外请求模型，只有显式输入才继续。
7. **原进程被杀**：已打开的观察端在宽限期后把 `running` 回合结算为中断，显示 `interrupted · duration unknown`，随后保持空闲、不重放、不自动重新请求。重新打开会话也会结算并提示 “the previous turn was interrupted before it completed and its request was not replayed”。只有显式 `/continue` 或新消息才发出一次新请求。
8. **执行所有权与准入**：恢复和观察只探测租约；结算只在取得租约后执行并立即释放，因此不会把仍在执行的回合标成中断。整个回合（包括反应式压缩、`Stop`/`PostCompact` 钩子、后处理和最终持久化）由宿主独占持有租约，引擎持有同一个 `Arc` 直到执行任务真正结束，包括公开 `MessageStop` 之后的收尾；宿主提前断开流或释放自己的引用不会让会话变成“无主”。独立提交（CLI、SDK、Desktop 或直接调用引擎）在发送入口就取得同一把锁，被占用时在保存输入、创建回合记录或请求模型之前立即拒绝；没有宿主租约时由引擎自己取得。委派子会话同样在执行器整个生命周期内持有租约，成功结束后才清除。准入是独占的：同一进程内第二个 Session 对象、或另一个进程的同会话顶层回合，都会被拒绝（跨进程 `flock`，进程内同样是独立描述符）。释放不依赖文件描述符关闭：`flock` 属于打开文件描述，`fork` 出的子进程会通过继承的描述符共享它，因此持有方结束时必须显式解锁（`fs::unlock_file`），否则锁会一直保留到子进程退出。租约在进程内只由取得它的进程解锁（记录取得时的 PID），子进程退出不会误放父进程仍在持有的锁；`execution_active` 探测成功后同样立即解锁再关闭。准入因此是立即且独占的，不再做 `WouldBlock` 重试。执行任务的完成记录按“尝试”身份登记，由任务自己在结束（含取消、panic、断流和提前出错）时删除；等待方只观察、不移除，因此 Desktop 等从不等待的调用方不会积累记录，旧尝试的清理也不能删除或等待同一会话的更新尝试。`wait_for_executor` 返回明确的 `Completed`/`Pending`：超时必须报告未完成并保留租约直到真正结束，不能把截止时间当成完成。取消语义同样以“强制工作真正结束”为准：外层执行任务被 abort 时，已经开始且无法中止的强制写库任务（`save_response` 的 assistant 消息与计数）继续持有同一份租约和同一个完成记录，因此取消返回后 `execution_active` 仍为真、独立提交仍被拒绝、`wait_for_executor` 仍返回 `Pending`，直到该阻塞任务退出才释放租约并报告 `Completed`；不能声称运行中的阻塞代码可以被终止。同一套语义适用于消息端点：`PresenceLease`（前端打开会话）和 `Consumer`（模型消息消费者）也在逻辑结束时显式解锁，并用取得时的 PID 保护，`session_open`/`receiver_active` 探测成功后立即释放自己的锁、`WouldBlock` 时不释放他人锁；`Consumer::attach` 在取得锁之后若建表/项目校验失败也会显式释放。生成行删除是尽力而为：`Consumer` 与所有 `Target` 克隆共享一个进程内生命周期标记，释放时先置为无效再清理数据库，因此即使删除失败，旧 `Target` 也不能再 `drain`，排队消息保持 `queued`；同一把锁同时串行化“正在进行的 drain”和释放，所以释放完成后不会再有旧句柄提交。新消费者用新的 generation 行，旧 generation 的清理不会删除它。`cargo test --locked -p deepseek-code-core --lib fs::file_lock`、`services::session_messages` 和 `executor_lifetime_tests` 用受控子进程继承描述符、复制描述符、执行器尾部闸门与写库任务内部闸门验证这一点。
9. 空闲恢复仍是第 6 条的 `Session restored; waiting for input`，不触发模型。

自动回归：

```bash
cargo test --locked -p deepseek-code-cli --test live_observation
cargo test --locked -p deepseek-code-core --lib db::turn_repo
cargo test --locked -p deepseek-code-cli --bin dcl observe::
```

`live_observation` 使用真实 CLI 进程、隔离数据目录和本地 SSE：证明运行中已发布工具/文本、消息 ID 不变、完成后测得耗时；`PostCompact` 钩子延迟期间租约仍在；杀死 owner 后恢复只结算、不重放，排队通知不被消费，显式继续才新增一次请求。准入与连续回合在 `--bin dcl` 中有确定性的进程内测试：`a_second_top_level_turn_is_rejected_before_any_record_or_request`（第一个模型的流被挂起，第二个顶层回合在任何记录或请求之前被拒绝）和 `rapid_sequential_turns_each_run_once_and_release_the_session`（连续 6 个回合各一次请求、各自完成）。执行器生命周期在 `cargo test --locked -p deepseek-code-core --lib executor_lifetime_tests` 中用一个只在测试中存在的尾部闸门验证：公开 `MessageStop` 之后、任务收尾之前，即使宿主断开流并释放自己的租约引用，`execution_active` 仍为真、独立提交仍被拒绝且不新增请求/消息；等待超时返回 `Pending` 且不释放；释放闸门后 `wait_for_executor` 返回 `Completed`，会话立即可用；取消、提前出错、无人等待的完成记录清理，以及旧尝试不能删除新尝试的记录也分别有确定性用例。另有一个真实成功尾路径的用例：在 `save_response=true` 的 assistant 持久化阻塞任务内部、实际写库之前设闸门，取消外层任务后仍保持租约、拒绝独立提交、报告 `Pending` 且保留完成记录；释放闸门后写库恰好发生一次，随后才释放并允许新回合，旧回合不会在新回合准入后再写库。终端显示必须用真实 pty 验收：

```bash
python3 dev-support/deepseek-code/super-agent-20261110/harness-fix/h1_pty_acceptance.py <本次构建产物> \
  --out <新的空目录>
```

脚本写入独立的 `results.json`，覆盖上述 1–5 条与正常结束、被杀两类结束方式：观察在附件之后才产生的文本标记、同一观察端跨过完成和被杀的显示、无重复行、排队通知保持不变、退出终端不影响 owner、无换行长文本可见。计数的前提是只统计该回合的请求；仅为会话标题或输入建议发出的请求不属于回合，脚本按请求体把标题请求单独分类，并关闭输入建议。

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
3. `cargo test --locked -p deepseek-code-core --test compaction_truncation`（41 项）：两个协议下首次摘要请求直接使用所选模型配置的输出上限（缺失或 0 时用 32768 固定回退）且篇幅目标不变、模型上限下按 token 二分并合并、合并再次截断后按同一预算拆分、合并持续截断停在深度上限并复用已完成分段、单段额度耗尽时其它分段继续完成、恢复合并超过 64K 时拒绝发送、恢复结果在失败和重启后复用、合并超出安全输入预算、最后实际预算与拟请求预算分开报告、窗口过小与分段规划失败的完整诊断、非截断立即停止、截断文本不进入缓存、缓存写入失败不提交、并发修改历史拒绝提交、局部摘要截断、约 85 万 tokens 历史的分段与分层合并、约 80 万 tokens 的单段失败隔离与缓存复用、长摘要形成小于 8 份的分组并单项传递、空正文按同一预算重试一次后安装、空正文重试仍为空时进入拆分、超长正文直接拆分且只按同一预算拆分不原样重发、拆分恢复返回空正文时先在同一预算原样重试、重试成功时不再拆分、重试仍为空时按同一预算拆分、拆分恢复返回超长正文时按同一预算拆分且不再重试、持续空正文停在深度上限且不写缓存、持续空正文与持续超长正文的最终错误保留结束原因、字符数、预算与用量、空正文导致的分段失败不影响其它分段并在重试时复用它们、大量分段共用额度时拆分请求未发出仍报告触发它的超长正文、空正文重试后再拆分同样保留诊断、取消后新增真实记录时内容未变的分段不再重发（同进程）、同一场景在关闭并重新打开数据库与引擎后仍只重发改变的分段。多数用例在 OpenAI 与 Anthropic 各跑一遍；`persistent_empty_summaries_stop_at_the_depth_limit_and_cache_nothing` 只跑 OpenAI。
4. `cargo test --locked -p deepseek-code-core --test compaction_overflow`（7 项，1 项按设计忽略）：分段、合并与最终摘要均从首个请求使用所选模型配置的输出上限；显式 8192 配置的每个请求都是 8192；配置为 null 或 0 时首个真实 HTTP 请求使用 32768 固定回退并仍完成提交；2000-token 窗口放不下该回退时发送 0 个请求，历史与缓存不变；原有取消、失败与最终摘要预算场景保持。
5. OpenAI 的 `length` 与 Anthropic 的 `max_tokens` 都按结构化结束原因识别，不依赖错误文字；其他结束原因仍按普通失败处理，不进入拆分，且不新增请求。正常结束但正文为空或超过 32,000 个字符同样是结构化结果，不当作输出上限，也不会保存或替换上下文。
6. 单次摘要请求的完整输入（系统指令、阶段提示词和历史）不超过 64,000 tokens，即使模型窗口能装下整段历史也先分段。该上限覆盖恢复合并：两份子摘要拼起来超过 64K 时，即使 1M 窗口仍装得下，也拒绝发送并只让该分段失败。约 85 万 tokens 的合成历史产生 14 个左右的分段，每个请求都在上限内，随后经 3 次分层合并得到最终摘要。
7. 预算与篇幅目标分离：分段与合并始终约为 2048 tokens，最终摘要始终约为 4096 tokens；输出预算为 32768/32000/128000 时也不放大目标。截断后不再提高预算，直接按同一预算拆分。模型上限为 4096 时目标随预算缩小，拆成两段后合并，内容顺序、中文、emoji 与超长工具结果完整。
8. 每个初始分段有独立恢复额度（通常 8 次），成功分段的剩余额度转入合并池；合并池初始 16 次，每个中间合并或最终摘要节点最多使用 8 次。某段额度、拆分深度或输入预算耗尽时，错误列出失败分段的数量与索引、该段拟请求与最后实际发送的预算、已用次数与原因；其中因“无法发出下一个请求”而结束的恢复，还会保留触发它的那次响应（分段、结束原因、字符数与字符上限、实际预算、已报告或未知用量），不会退化成只说明额度用尽。其它分段继续完成并写入缓存，`/history` 中的原始记录不变，缓存中没有截断文本。
9. 合并按时间顺序分层，每组最多 8 份摘要且不超过单次输入上限；单剩余项直接进入下一层；摘要数量无法减少、或连两份摘要都放不进合并输入预算时明确失败，不发送超限请求、不循环。
10. 恢复过程中关闭并重新打开数据库与引擎后重试，或中途取消后重试，已完成的分段与子摘要都直接复用；恢复期间历史发生变化时拒绝提交，原有记录与新增记录都保留。
11. 手动 `/compact` 与自动压缩显示同一套逐段进度。
12. 真实 ConPTY：相邻开发支持目录的 `compaction-recovery-20261107/compaction_truncation_acceptance.py` 用隔离数据目录和本地模拟接口。每次运行写入独立目录（`--out`，或 `runs/<时间>-<随机>`），目录内保存 `run.json`（二进制路径、SHA256、运行时源码 revision 与是否有未提交改动、相关源码与验收脚本的 SHA256、场景参数、起止时间）和 `results.json`；非空目录直接拒绝，不会覆盖旧证据。运行时 revision 只说明当前检出，不能当作二进制的构建来源。每个场景声明必须产生的检查项；缺少必需项、任一项为假或场景报错都判为失败，重启未就绪与取消被忽略都不能跳过。场景：
    - `auto`、`manual`（窗口 14,000）：超大历史、两段摘要、首段截断。断言原始分段恰为 2、恢复子段恰为 2、合并在子段之后，核对终端 `fragment 1/2` 与 `fragment 2/2`；自动场景还检查 `Auto-compacting conversation`、压缩后原请求继续且只执行一次并携带摘要，手动场景检查 `/compact` 的进度与 `Compacted in … (ctrl+o to see full summary)`。
    - `large`、`large-manual`（窗口 100,000）：历史约 86k tokens，整段能放进 90% 安全线但超过 64K 单次上限，因此仍拆成两段；摘要请求使用配置的输出上限，截断请求按同一预算拆分出的子段正常完成。断言所有摘要请求不超过 64K、至少两次截断被恢复且后续请求预算不变、进度可见；自动场景还验证原请求只继续一次并携带摘要。
    - `empty-auto`（窗口 100,000）与 `empty-retry-manual`（窗口 14,000）：首段在同预算下返回空正文，随后在同一预算原样重试成功。断言两次请求预算相同、请求体完全相同、该分段之后的所有请求都重复同一输入（没有拆分）；自动场景还要求原请求只继续一次并携带摘要。
    - `oversized-retry-manual`（窗口 14,000）：首段截断、随后返回 32,001 字符。断言不再提高预算，后续拆分请求使用同一配置预算、两个拆分输入与第一个请求不同，压缩完成并替换历史。
    - `empty-manual`、`blank-manual`（窗口 14,000）：普通入口直接返回空正文或纯空白正文，断言第二次请求与第一次同预算、同请求体，成功后替换历史，`/compact` 显示完成提示。
    - `oversized-manual`（窗口 14,000）与 `oversized-auto`（窗口 100,000）：32,001 字符正文。断言没有原样重试、输出预算不变、后续请求是更小的拆分输入，压缩完成并替换历史；自动场景还要求原请求只继续一次。
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

### N. 技能目录发现与优先级

模型可见的技能列表、`Skill` 工具和斜杠列表共用 `skills::discovery::discover(data_dir, workspace)`。阅读顺序（后者覆盖前者）是：内置工作流（`bundled.rs`）、`CLAUDE_CONFIG_DIR`（未设置时为 `~/.claude`）、`<数据目录>/skills`、`<工作区>/.claude`。同一名称只列一次；项目覆盖用户，数据目录覆盖 Claude 兼容用户目录。`<数据目录>/skills/bundled` 和 `mcp` 仍会列出（斜杠列表原本就显示它们），但不会覆盖已发现的内置或 Claude 用户同名技能；`<数据目录>/skills/<名称>` 的直接用户技能仍按“用户覆盖内置”处理。发现过程只读，不创建 `<数据目录>/skills`，不跟随符号链接。

数据目录技能的名称沿用 round 01 之前的斜杠命令规则：取 frontmatter `name`（缺省时为技能所在目录名）转小写、空白折叠为 `-`，其他标点保留。因此 `My Skill` 仍是 `/my-skill`，`R&D (team)'s check` 仍是 `/r&d-(team)'s-check`；多层集合也只使用技能自身名称，不加集合前缀。两个数据目录技能归一化后同名时，两者都改用按 `SKILL.md` 路径计算的稳定 `skill_<hash>`（只有哈希碰撞时才按排序后的路径追加 `-2`、`-3`），不会按文件系统顺序只保留其中一个。`~/.claude`、项目与插件技能的名称规则不变。

1. 自动化：`cargo test --locked -p deepseek-code-core --lib discovery::tests`、`cargo test --locked -p deepseek-code-core --lib data_dir_skill`、`cargo test --locked -p deepseek-code-core --lib runtime_discovery_keeps_pre_round_01_slash_names_for_mixed_case_and_spaces`、`cargo test --locked -p deepseek-code-core --lib runtime_discovery_keeps_punctuation_that_skill_creation_allows`、`cargo test --locked -p deepseek-code-core --lib runtime_discovery_disambiguates_shared_data_dir_names_with_stable_ids`、`cargo test --locked -p deepseek-code-cli --bin dcl slash_list`、`cargo test --locked -p deepseek-code-cli --test skills`。真实进程用例确认 `CLAUDE_CONFIG_DIR` 指向空目录时，首个请求的 `Available skills` 含数据目录技能（名称已小写化），第二请求的工具结果给出真实技能目录。`discovery::tests` 中的 `installed_placeholder_and_legacy_builtins_are_visible_to_both_views` 还确认 `ensure_installed` 不再写出 `skill-creator` 占位，内置 `skill-creator` 只在模型列表和斜杠列表中各出现一次，五个 legacy builtins 仍按原样可见。
2. 真实终端：用空的 `CLAUDE_CONFIG_DIR` 和独立数据目录，把 `skills/<名称>/SKILL.md` 只放在数据目录，运行 `dcl --data-dir <目录> --print "/<名称> 参数"`。确认斜杠命令展开、`dcl skills list` 显示该技能；再让模型自行调用 `Skill`，确认工具名与斜杠名称一致，返回的说明包含实际数据目录路径，`references/...` 等相对路径可解析。
3. 同名覆盖：在数据目录、`CLAUDE_CONFIG_DIR` 和工作区 `.claude/skills` 放同名技能，确认列表只有一项且三处内容按“项目 > 数据目录 > CLAUDE 用户”选择；移除高层副本后依次落到下一层。
4. 开关：`enabled: false` 不出现在列表和工具中；`disable-model-invocation: true` 不在模型列表、`Skill` 工具也不能调用，但斜杠命令仍可用；`user-invocable: false` 仍只供模型调用。
5. 容错：在同一目录放一个格式错误的 `SKILL.md` 和一个有效技能，确认有效技能仍可用，无效文件只产生路径诊断（运行日志），不会回显文件内容；`<数据目录>/skills` 不存在时发现过程不创建它。
6. `--data-dir` 与 `DEEPSEEK_CODE_DATA_DIR`：两者选中的非默认数据目录都生效，模型列表和工具使用同一位置，不回退到 `~/.deepseek-code` 或 `~/.claude`。
7. 内置工作流不得被意外覆盖：在 `<数据目录>/skills/bundled` 放与内置同名的技能，确认仍执行内置定义；直接放在 `<数据目录>/skills/<同名>` 的用户技能按既有规则覆盖内置。插件命名空间技能的行为不变。
8. 名称兼容：把 frontmatter `name` 写成 `My Skill` 或含 `(`、`&`、`'` 的名称，确认斜杠命令分别为 `/my-skill`、保留标点的旧名称，模型列表和 `Skill` 工具使用同一个名称，不存在大小写不同的第二个别名。再放两个同名的数据目录技能，确认两者都按 `skill_<hash>` 列出，重复运行结果相同。

### O. 内置 skill-creator

1. `dcl skills list` 在全新数据目录和已有旧占位的数据目录中只列出一个 skill-creator。旧占位必须是 `SKILL.md` 加 `agents/analyzer.md`、`agents/grader.md` 且内容未被改动才会被删除；改动过的副本、额外文件和不相关兄弟技能保持原样。
2. 模型通过 `Skill` 工具调用 skill-creator：返回的说明含 `Base directory for this skill:` 和本次物化的临时目录；目录内有 `SKILL.md`、`scripts/init_skill.py`、`scripts/quick_validate.py`、`license.txt`、`NOTICE.md`、`references/frontmatter.md`，内容与内置字节一致。断网、未预先安装时同样可用。
3. 在真实终端执行 `/skill-creator 创建一个处理 CSV 的技能`：走同一执行路径，命令表只出现一次 `/skill-creator`。
4. 用没有 PyYAML 的 `python3` 运行物化后的 `scripts/init_skill.py demo --path <临时目录>`，再运行 `scripts/quick_validate.py <临时目录>/demo` 必须通过；缺少 name/description、name 含空格或 frontmatter 未闭合时必须返回非零。
5. 自动回归：
   ```bash
   cargo test --locked -p deepseek-code-core --lib skills::skill_creator
   cargo test --locked -p deepseek-code-core --lib skills::bundled_resources::tests
   cargo test --locked -p deepseek-code-core --lib skills::bundled::tests
   ```
   覆盖发现恰好一次、模型和用户都可调用、迁移只删除原生占位、物化字节和完整性检查、Python 脚本在 `-S`（无 site-packages）下创建并通过验证。单元测试不能代替第 2、3 条的真实进程调用。

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
- `repl.rs`：C4、D1、D2、D6、D7、D8–D10（恢复提示、完成耗时与运行中状态），以及 L（手动 `/compact` 的完成输出、提交前取消与 `post-commit-manual` 的提交后取消）
- `terminal/input.rs` 的取消通道：L 的 `cancel-manual`、`cancel-empty-manual`、`cancel-invalid-manual`、`cancel-merge-manual`、`post-commit-manual`，并在真实终端确认 `Esc` 不滞留到下一个提示、等待期间 `Ctrl+O` 仍能打开完整记录
- `status_commands.rs`、`services/context_usage.rs`：`status_commands_tests`、`compaction_status_regressions` 与 L 的 `status-manual`（按边界与配置匹配用量、部分估算、压缩后估算、次数持久化）
- `session_commands.rs`：L（自动压缩完成输出与次数）
- `worker_frontend.rs`、`background_commands.rs`：B、D、E3、E4、F
- `memory.rs`、`display_commands.rs`、`crates/core/src/memory`：G
- `services/customization.rs`、`prompt/`、`native_engine/tool_loop.rs`：H，以及 SDK 测试
- `crates/core/src/skills/discovery.rs`、`crates/core/src/native_engine/engine_core.rs` 的技能列表、`crates/core/src/native_engine/skill_tool.rs`：N
- `crates/core/src/skills/mod.rs` 的 bundled 安装、`bundled.rs`、`bundled_resources.rs`、`bundled_manifest.rs`、`skill_creator.rs`、`skills/bundled/`：O（安装与 `ensure_installed` 的旧占位迁移见 N、O 两节）
- `tools/cron_jobs.rs`、`tools/wakeup.rs`、`main.rs` 的空闲循环：I
- `terminal/picker.rs`：F，以及 `/resume`、`/rewind`、`/mcp` 等使用选择列表的命令
- 欢迎区生命周期、输入框与运行帧交接、`/plan` 提示：J，以及 B、C、E1、E3
- `session.rs`：B、D（含 D8–D10）、E；SDK 输入路径另见“第十一轮：SDK 回合内引导”；回合内的执行所有权（含反应式压缩与钩子）按 D8 第 8 条，另跑 `cargo test --locked -p deepseek-code-cli --test live_observation ownership_survives`
- `db/turn_repo.rs`、`services/session_messages.rs` 的回合状态与执行租约、`observe.rs` 的只读观察：D8–D10，以及 `cargo test --locked -p deepseek-code-cli --test live_observation`、`cargo test --locked -p deepseek-code-core --lib db::turn_repo`、`cargo test --locked -p deepseek-code-cli --bin dcl observe::`；恢复/租约回归另跑 `cargo test --locked -p deepseek-code-cli --test session_worker --test session_messages`；`crates/core/src/native_engine/engine_core.rs`、`tool_loop.rs` 的租约字段类型改动另跑 `cargo test --locked -p deepseek-code-core --lib native_engine::`
- `services/session_worker*`、`services/background_sessions.rs`：D、E4，以及 `cargo test --locked -p deepseek-code-core --test worker_environment`（普通子进程不继承 worker 的控制变量）
- `tools/coordination.rs`、`native_engine/tool_loop.rs`、`native_engine/plan_tool.rs` 的计划入口退役：`cargo test --locked -p deepseek-code-core --lib native_engine::tool_loading_tests`、`cargo test --locked -p deepseek-code-core --lib native_engine::plan_tool::tests`、`cargo test --locked -p deepseek-code-core --lib tools::coordination::tests`（四个任务内置各一次、`TaskOutput` 与 `plan_mode` 不在目录/请求/搜索中；旧设置启用和旧 ToolSearch 记录都不能恢复 `plan_mode`；直接调用报 Unknown tool 且不创建 `.claude`；`EnterPlanMode`/`ExitPlanMode` 审批与计划边界仍通过）
- `tools/plan_import.rs`：`cargo test --locked -p deepseek-code-core --lib tools::plan_import`（显式内部导入：真实 legacy SQLite → 会话任务库，状态/结果/反向依赖保留且重开可读；dry run 与缺失/非法源不创建目标；未知状态/重复步骤/悬空/自引用/成环/超限与中途插入失败整体回滚；同源哈希幂等并保留手工编辑；源变化冲突不写；映射任务缺失报冲突；同一 plan UUID 跨 workspace/session 隔离；并发只建一套；current 标记只在同 workspace/session 内更新；`current_plan` 单独变化（记录 body 不变）两个方向都判为冲突且不写入；不相关计划的 current 切换仍是 unchanged；旧版哈希格式报明确冲突且不重建映射行）
- `crates/cli/tests/tasks_catalog.rs`：`cargo test --locked -p deepseek-code-cli --test tasks_catalog`（真实 CLI + 本地 mock：四个任务工具默认不可见，经逗号和空格两种 ToolSearch 选择后在下一请求出现；核对实际请求 schema，并通过真实任务工具创建根/步骤、设置依赖和输出、列表与读取；持久化行/边/结果与 `TaskGet` 一致，请求中没有 `plan_mode`/`TaskOutput`）
- `crates/cli/src/task_commands.rs`、`args.rs` 的 `tasks import-plan`：`cargo test --locked -p deepseek-code-cli --test tasks_import`（真实 `dcl` 子进程 + 本地临时 legacy/数据目录：成功导入后可用任务存储读回根/步骤/依赖；dry run 与缺失/非法源、非法会话/计划、缺少或同时给出选择器都不创建目标；仅 current 变化返回 conflict 且退出码 1、任务不变；`--data-dir` 隔离；workspace 旧库保持只读）
- `tools/tasks.rs`、`tools/background_shell.rs`、Workflow/背景 Agent 的 TaskGet 输出：`cargo test --locked -p deepseek-code-cli --test taskget_backends --test taskget_background`（真实 CLI + 本地 mock：Workflow 实时输出、完成、超时不取消、TaskStop 取消；背景 Agent 运行快照、报告、退出码、超时不停、会话隔离；请求中四个任务内置各一次且无 TaskOutput，并核对实际请求的 TaskUpdate schema 带 `setBlockedBy`），以及 `cargo test --locked -p deepseek-code-core --lib tools::tasks` 与 `cargo test --locked -p deepseek-code-core --lib tools::background_shell`。`tools::tasks` 覆盖 `TaskUpdate setBlockedBy` 的原子替换/清空、反向 `blocks` 同步、重复项去重、缺失/自引用/成环/跨会话拒绝且失败不落盘、`addBlockedBy`/`addBlocks`/删除混用时先拒绝。
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
- `skills/discovery.rs`、`native_engine/skill_tool.rs`、`native_engine/engine_core.rs` 的技能发现与模型列表、`cli/skill_commands.rs`：N
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
5. 后台任务运行时，`TaskGet(任务名称)` 在 `block:true` 等待结果前出现；恢复历史后仍显示名称。
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

### 会话创建与消息追加的 SQLite 写者竞争

- `cargo test --locked -p deepseek-code-core --test session_concurrency`：`SessionStore::create`/`append` 在第二个连接持有写锁且超过一次 `busy_timeout` 时，应重试整笔事务而不是直接失败；持锁超过 30 秒预算时返回带静态操作名（`session create`／`session append`）的类型化竞争错误，不得留下部分会话、项目、文件夹、消息、结构化内容或来源行，释放写者后同一连接池仍可正常写入。
- 非竞争错误（如触发器中止）必须立即返回并整笔回滚；`BEFORE INSERT` 早失败和“消息/计数写入后、最终时间戳更新时失败”两种中断点都要覆盖。
- 口径：每次尝试仍使用每连接原有的 5 秒 `busy_timeout`；30 秒只是整笔事务的重试预算，不改变全局超时，也不序列化整个应用。测试里夹具持有写者的保护上限长于该预算，避免夹具与预算同时到期。
- 未包含：`derive_empty`（fork/clear/子会话）仍是单次 `BEGIN IMMEDIATE`，本轮没有观察到失败，也未改动。
- 不确定：R37 全量套件失败只在“写者被占用超过一次 busy_timeout”这一边界上被确定性复现；保留的日志不能证明当时写者为何 ≥5 秒不可用（调度饥饿还是其他长写），因此不声称已还原那条历史时间线。

### 回合内控制请求的确定性夹具

- `cargo test --locked -p deepseek-code-cli --test sdk controls::running_turn_answers_queries_and_cancels_only_queued_messages -- --exact`：`Mode::HeldText` 只在第一个流式请求上先发出该协议的第一帧（Anthropic `message_start`／OpenAI 首个 delta），随后等待测试释放的 `Notify` 才发送完成帧；控制序列在释放前执行并刻意等待超过旧的固定 2 秒，两个协议都必须回答全部控制、保持排队消息未运行，释放后恰好 2 个结果、被取消的第二个提示不出现、保留的第三个提示出现。
- 闸门等待有 30 秒上限，超时以 `io::Error` 结束响应体使测试显式失败，不会静默回落到旧的固定延时语义；测试自身还有 60 秒看门狗，`ReleaseHeld` 在断言失败或 panic 时也会释放。
- 旧固定延时的负向复现不保留为源码里的测试：用例原文与重建步骤见相邻开发支持目录 `session-sqlite-20261111/round04-negative-control-test.rs.txt`，临时加入后以 `--ignored`（或普通）运行应看到 `Unexpected terminal result while waiting for control_response`，这正是 2026-11-11 全量 CLI 的失败形态；负向日志为同目录 `logs/round04-sdk-negative-control.log`。一次串行基线通过不能证明旧竞态不存在。

### 会话消息收件箱的 SQLite 竞争与有界轮询

会话消息（`SendMessage`/`session_messages`）的引擎侧轮询发生在每个模型/工具边界。宿主同时会把完整对话写回同一个数据库，因此轮询不能因为一次写者竞争而结束整个回合。

```bash
env -u DEEPSEEK_CODE_BACKGROUND_CONTROL -u DEEPSEEK_CODE_BACKGROUND_JOB \
    -u DEEPSEEK_CODE_WORKER_MODEL -u DEEPSEEK_CODE_WORKER_AUTO_ROUTE \
    -u DEEPSEEK_CODE_CHILD_API_KEY -u DEEPSEEK_CODE_DATA_DIR \
  cargo test --locked -p deepseek-code-core --lib -- \
    services::session_messages::tests:: db::write_tx_tests::
```

- `an_empty_drain_does_not_take_the_writer_lock`：第二个真实 SQLite 连接持有 `BEGIN IMMEDIATE` 时，空收件箱轮询不等待写者、不报错。空收件箱的授权检查在只读快照中完成，仍验证消费者 generation 以及 `project()`/`resolve_workspace` 的归档、Code 模式、项目信任和来源映射。
- `an_empty_inbox_still_enforces_project_authorization`：空收件箱下分别归档会话、改成非 Code 模式、撤销项目信任且 generation 不变，三种情况都必须像旧路径一样拒绝，且不消费任何消息。
- `a_pending_drain_defers_under_a_held_writer_and_delivers_once_after_release`：有排队消息但写者被占用时，轮询在限定时间内返回空；队列行保持 `queued`，`consumer_generation`/`committed_at` 保持空，历史为空。释放写者后同一条消息只投递一次，状态变为 `context_committed`，历史恰好一行，再轮询为空。
- `a_late_acknowledgement_failure_rolls_back_the_whole_poll_unit`：在消息行插入和会话计数增加之后、`session_inbox` 确认更新时让触发器失败；整个轮询事务必须回滚（历史为空、`message_count` 不变、队列状态/generation/时间戳不变）。移除触发器后恰好投递一次。早期的 `BEFORE INSERT` 失败用例保留，覆盖更早的中断点。
- `a_nonbusy_drain_failure_names_the_operation_and_keeps_the_queue`：非竞争错误（`RAISE(ABORT)`，扩展码 1811）仍向上传播，消息保持 `queued`，后续轮询只投递一次。
- `a_revoked_consumer_is_refused_and_the_queue_survives`：消费者被撤销时轮询拒绝，队列不丢失；新消费者只投递一次。
- `the_poll_restores_the_borrowed_connection_timeout_on_success_and_unwind`：借用连接原来的 `busy_timeout`（例如 7000ms）在成功、错误和 panic 展开后都恢复为原值，不留下 250ms 或全局放宽的超时。
- `db::write_tx_tests::contention_exposes_the_extended_code`：真实写者竞争下诊断包含静态操作名和 SQLite 扩展码，不含 SQL 文本或绑定值。

口径说明：

- 只读快照读取沿用该连接原有的 `busy_timeout`（池默认值），250ms 只是有排队消息时“获取写事务”的单次上限，不是一次轮询或整个回合的总时限；模型请求、工具执行和其他写路径不受影响。
- 仅对 `SQLITE_BUSY`/`SQLITE_LOCKED` 的类型化错误回滚并推迟到下一个边界；所有权、信任、归档和写入错误仍作为错误返回。没有递归重试，也没有全局修改数据库超时。
- 本轮只证明并修复了收件箱轮询这一条可复现的致命竞争路径。历史长回合退出（Super R27/R01/R28）和独立数据目录下的启动期失败仍未归因；启动期数据库打开/初始化也未加同样的操作名诊断，不能用本场景的通过结果替代。
### Super Agent 0–2 阶段：子会话、任务存储与隔离

本轮只覆盖 Router 之前的基础能力；Coder/Reviewer 调度、`/crp` 和 Desktop 入口尚未实现，不能按完整 Super Agent 验收。

- `cargo test --locked -p deepseek-code-core --lib super_agent`：任务规格、状态机、完成门禁、存储、仓库发现和隔离共 64 项。重点检查：
  - 损坏的 `.git` 指针、损坏仓库、Git 不可执行和不可读元数据都是 `DETECTION_FAILED`，不会降级为“无 Git”；已有文件目标从其所在目录探测，尚不存在的文件目标从最近存在的目录探测。
  - 审核报告的信封与正文版本、轮次必须等于任务当前值；通过结论必须覆盖每条验收标准、不得重复或引用未知标准、必须有证据；空的提交/审核/集成版本不被接受；会话必须在数据库中真实存在，执行轨迹必须包含实现、审核和 Harness 事件；非 Git 快照必须通过内容校验。
  - 上一轮 `BLOCKED` 或退回的报告保留为历史，不会永久阻止后续轮次；只按当前 `spec_version` 和 `review_round` 判断。
  - 锁只按当前 `sa_locks` 判断所有权：释放后的旧 operation 重放返回 `Released`，不会被当作仍持有；非 Git 路径锁按规范化路径的祖先/后代关系冲突，`/target` 与 `/target2` 互不冲突；独立数据库连接的并发只有一个持有者。
- `cargo test --locked -p deepseek-code-cli --test super_agent_child`：真实 `dcl` 子进程加本地模型 mock（不连接真实服务）。子进程用 Bash 工具把 `pwd` 写入临时文件，验证首次和 `--resume` 后都在准备的工作目录；`conversations.workspace_path` 记录该目录；父工作目录没有子进程写入；旧的 Job 已完成、没有重复的活动子进程；尝试把已有子会话移到另一个目录会被拒绝。
- `cargo test --locked -p deepseek-code-core --lib native_engine::child_session`：抽取后的启动/继续逻辑 7 项，覆盖无效父会话、归档父会话、启动失败不写成功状态、重新使用同一会话和拒绝更换工作目录。
- `cargo test --locked -p deepseek-code-core --lib native_engine::tool_loop::delegation` 与 `cargo test --locked -p deepseek-code-core --test delegation`：原有 Agent 委派和后台子会话行为不变（分别 22 与 4 项）。
- 非 Git 快照场景包含在 `super_agent::isolation`：用户自己的 `manifest.json`、文件与目录互换、可执行权限、快照目录位于源目录内或其符号链接别名的拒绝、损坏或缺失 payload 时先校验后恢复（当前文件保持不变）、被跳过条目（依赖目录等）在清单中如实记录而不是声称已保护。
- 当前无法自动化的部分：真实多仓库任务的合并与清理、Router 与 `/crp` 交互、Desktop 角色切换、Windows 上的路径与符号链接行为，均在后续轮次实现并单独验收。

### Super Agent 第 3 阶段：角色、Router 工具与 CRP 启动

本轮覆盖角色与工具授权、任务持久化事件和 Coder/Reviewer 启动。审核、集成、Watcher、`/crp` 页面和 Desktop 入口属于后续阶段，不能按完整流程验收。

- `cargo test --locked -p deepseek-code-core --lib super_agent`：设置校验、角色持久化、CRP 工具授权、事件待处理/确定性处理、启动器与编排器共 93 项。重点检查：
  - 角色默认 Agent，按会话持久化；`--role` 只在显式传入时覆盖，恢复会话时沿用已保存角色；角色与权限模式互不影响。
  - Router 会话的工具表中没有 `Edit`、`Write`、`MultiEdit`、`NotebookEdit`，但仍保留只读工具和 `Bash`；执行层同样拒绝这些工具（计划文件的既有例外保留），伪造调用不能绕过。
  - `CrpCreate`/`CrpList`/`CrpShow`/`CrpSend`/`CrpUpdateSpec`/`CrpAnswer` 只对 Router 开放且只作用于本 Router 的任务；`CrpAnswer` 只能回答存在、待处理、规格版本与权限策略版本都匹配的审批，重复回答同一 operation 返回原结果。
  - `CrpSubmit`/`CrpProposeTask` 只对任务当前 Coder/Reviewer 开放；Coder 只能提交 plan/implementation/integration，Reviewer 只能提交 review/harness；报告正文与当前 `spec_version`/`review_round` 不符会被拒绝。成员工具只写待处理事件，不直接改变任务状态。
  - `ingest_event` 与 `process_pending` 分离：未处理事件不改变任务；重复处理同一事件是 no-op；非法事件让整批处理失败且不留下半应用状态；只处理本 Router 的待处理事件。
  - 编排器一次 tick 依次准备环境、以计划模式启动 Coder、按 `manual`/`auto` 策略创建或自动批准计划审批、批准后恢复同一 Coder；重复 tick 不产生重复会话、Job 或审批。配置的 Coder 模型不存在时回退 Router 模型并记录警告。
  - Reviewer 使用按 `submitted_commit` 固定的独立工作树，与 Coder 可写工作树不同。
- `cargo test --locked -p deepseek-code-cli --test super_agent_router`：真实 `dcl` Router 会话调用 `CrpCreate`；宿主 tick 启动真实 `dcl` Coder，其本地模型 mock 调用 `CrpSubmit(plan)`；tick 生成审批；第二次真实 `dcl` 会话调用 `CrpAnswer`；tick 恢复同一 Coder。仅使用本地 mock，检查任务归属、阶段从 PLANNING 变为 IMPLEMENTING、会话 ID 不变、子进程工作目录为任务工作树、父工作目录无写入且没有重复活动子进程。
- `cargo test --locked -p deepseek-code-cli --test super_agent_child`：现有 Agent 子会话的真实工作目录与恢复行为不变。
- 现有回归：`cargo test --locked -p deepseek-code-core --lib native_engine::tool_loop::delegation`、`cargo test --locked -p deepseek-code-core --test delegation`、`cargo test --locked -p deepseek-code-cli`。
- 当前无法自动化的部分：`/role` 交互渲染、等待用户输入时的手工恢复、参考文档第 13 节的完整配置项（本轮只读取 `router`、`agents`、`approvals.plan`、`review.maxRounds`、`scheduler` 中已被使用的字段）、Windows 下的工作树与进程行为、真实多仓库与审核/集成流程。

### Super Agent 第 4 阶段：审核请求、Reviewer 查询与报告配对

本轮覆盖提交版本固定、Reviewer 独立工作区、只读会话查询、报告配对和退回重审。集成、Watcher、`/crp` 页面、Desktop 入口和完整端到端仍未实现，不能按完整流程验收。

- `cargo test --locked -p deepseek-code-core --lib super_agent`：设置、角色、CRP、审核请求、Reviewer 查询、隔离和编排共 110 项。重点检查：
  - 任务规格的 `workspace` 与每个目标必须位于 Router 持久化工作区和 `settings.json` 的 `permissions.additionalDirectories` 之内；自称的规格工作区或外部目标在创建任何工作树/快照/子进程之前被拒绝，显式授权目录则被接受。
  - 实现提交时检查任务工作树 `git status --porcelain` 为空并固定实际 `HEAD`；有未提交修改时任务阻塞且不启动 Reviewer。Round 增加后会重新固定新 `HEAD`，并使用 `agent/<task>-review-<round>` 的独立工作树。
  - `review_request` 记录当前 `spec_version`/`review_round`、实际提交版本、事件序号范围、Coder 当前/历史/递归子会话、证据引用和 Harness 版本；非 Git 目标记录内容差异并生成独立审核快照。
  - Reviewer 只能查询本任务 Coder 会话树；`SessionMessages`/`SessionEvents` 使用稳定游标分页，输出脱敏；`ArtifactRead` 只接受报告 ID 或任务 artifacts 目录中的普通文件，拒绝遍历、符号链接和跨任务路径。
  - `CrpSubmit` 的 `payload` 暴露 plan/implementation/review/harness/integration 五种类型化 schema；Review 报告必须匹配当前阶段（`IMPLEMENTATION`/`FINAL_DELIVERY`）。Review 与 Harness 报告都到达后才作结论，与到达顺序无关；缺少任一份不推进任务。
  - `CHANGES_REQUESTED` 增加轮次并把具体 issues 发回同一 Coder 会话；新实现提交后同一 Reviewer 会话被恢复并使用新的审核请求。`BLOCKED` 或达到配置阈值的 Harness 故障创建 `DELIVERY_BLOCKER` 审批；仅效率类建议不阻塞。
  - 初始启动和每次恢复按 task/member/spec/round 持久化一个操作，并在任何外部进程之前预定会话与 Job UUID；重新打开数据库后按该身份继续或接管，不新建第二个会话/Job，也不重发已消费的指令。
  - 保留 Job 在 `Command::spawn` 之前发布 `worker.claim`（独占执行令牌），spawn 成功后才写不可变的 `spawn.json`；`state.json` 在启动前由宿主写入预约，之后只由真实 supervisor 写入。快速子进程写出的 `completed`/`failed` 不得被宿主的旧 `starting` 快照覆盖。
  - 接管已推进的 Job 之前，必须校验父会话、子会话、启动参数和规范化工作目录；不匹配时保留原始文件并明确失败，不得因为状态已是 `launched`/`running`/`completed` 而提前返回。
- `cargo test --locked -p deepseek-code-core --lib services::background_sessions`：真实 supervisor（测试二进制以 `--exact --ignored` 调用生产 `run_worker_from_env`）加真实子进程。覆盖：终端状态在宿主发布后仍保留、spawn 后宿主中断的接管、重复 supervisor 因 claim 令牌不符而不执行子进程、身份/参数/工作目录不匹配在任意已推进状态下被拒绝、匹配的已推进 Job 被接管且不启动新进程。
  - 支持的配置路径为 `settings.json` 的 `superAgent` 对象（camelCase 字段，例如 `review.maxRounds`、`review.blockingSeverity`、`review.maxTaskProposals`）；测试验证非默认值实际生效。
- `cargo test --locked -p deepseek-code-core --lib services::session_messages::tests`：消息投递按 operation 幂等；重复 tool_use_id 或投递后崩溃重试只产生一条收件箱消息。
- `cargo test --locked -p deepseek-code-core --lib native_engine::tool_approval`：Coder 计划批准前 Write/Bash 在运行时被拒绝，计划文件仍可写；规格/策略不匹配的旧审批不能放行；当前审批放行，ExitPlanMode 切换到 Router 继承的权限模式。
- `cargo test --locked -p deepseek-code-cli --test super_agent_router`：真实 Router/Coder 计划审批流程（仅本地 mock）。Coder 在批准前尝试 `Bash` 被硬拒绝且不创建文件；提交计划并获批后，同一会话执行只读 `Bash` 得到正常结果，证明计划门禁只阻止未批准的执行。
- 当前无法自动化的部分：真实 Reviewer 模型进程的完整退回→修复→复审循环、非 Git 多目标端到端、Watcher、`/crp`、Desktop 和 Windows 行为；这些属于后续阶段，不能由本轮单元/编排测试替代。

### Super Agent 第 5A 阶段：集成准备与 shell 状态暂存回滚

工作区迁移会暂存会话已保存的前台 shell 工作目录。候选文件必须在任何重命名之前全部列出并校验，只接受精确的会话文件名和 `safe_cwd` 生成的精确 WSL 名（`<session>.wsl-` 加 16 位小写十六进制）；`.stale-*`、格式不合法的后缀和其他会话的文件都不能被选中。暂存第 N 个文件失败时，必须尝试恢复此前所有已暂存文件，并同时报告原始错误和每个回滚失败/保留的备份路径。

- `cargo test --locked -p deepseek-code-core --lib native_engine::tool_loop::shell_cwd_tests`：9 项。覆盖第二个有效重命名被注入失败后第一个文件恢复、回滚也失败时保留备份并报告路径、恢复遇到新建的活动状态时拒绝覆盖、hard link 失败且目的地在同一时刻被创建时绝不替换、恢复/清理逐个尝试并汇总失败、清理失败发生在数据库提交之后不回滚缓存、stale/大写/长度错误/非十六进制后缀与其他会话不被选中。
- `cargo test --locked -p deepseek-code-core --lib native_engine::workspace::tests`：12 项。除原有成功迁移、事务中止恢复和托管工作树拒绝外，新增真实 `shell-cwd` 不是目录时数据库完全不变，以及提交后的清理失败保持新工作区且不恢复旧缓存。
- `cargo test --locked -p deepseek-code-core --lib native_engine::child_session::tests`：7 项，恢复既有工作目录行为不变。
- `cargo test --locked -p deepseek-code-cli --test super_agent_review_loop`：3 项。真实最终审核阶段仍在集成提交的隔离检出中读取 `pwd`、文件内容和 `HEAD`，目标分支保持基线，未产生交付/完成/清理事件。
- 未验证：不支持 hard link 的文件系统只会得到明确错误并保留备份（没有覆盖式回退，可选可用性降级未实现）；进程崩溃时文件系统与数据库之间不承诺原子性；Windows 未运行。负向对照使用旧实现和新实现各自编译的独立探针，记录在开发支持目录的 `round13-evidence/negative-control/`。

### Super Agent 第 5B1 阶段：交付预检与分支所有权

本阶段只生成并持久化交付计划、取得协作式分支所有权，不写目标分支、不产生 `INTEGRATION_COMPLETED`、不清理工作树。目标必须是任务保存的精确 `refs/heads/<branch>`，不能回退到 `main`；基线、已审核提交必须都是集成提交的祖先，集成提交和树对象必须与主机验证过的 `INTEGRATION_PREPARED` 事实一致。计划只从主机事件和主机决定的已批准 Review+Harness 生成，模型报告里的成功布尔值不是证据。交付后验证每次都用 `git worktree list --porcelain -z` 重新发现当前目标分支的检出拓扑，不能因为计划当时是 `RefOnly` 就跳过检出检查。

- `cargo test --locked -p deepseek-code-core --lib super_agent::delivery`：44 项。覆盖非 `main` 目标、未检出分支的 CAS 计划、带空格路径的 linked worktree 检出、脏/未跟踪/锁定/缺失工作树、非 Git-only 任务、旧/新内容对实现阶段或最终交付阶段复用的选择、基线移动返回 reintegration、外部任务竞争同一分支、按规范锁键排序的多仓库获取、部分获取失败只释放本次新锁、SQLite 触发器拒绝保存计划时释放新锁并保持 Git 不变且同一内容用新 attempt 重试成功、陈旧 spec/round 或仓库集合不能通过交付验证、部分多仓库交付结果与“代理声明不覆盖真实目标 OID”。第 16 轮新增：交付命令的 CAS/快进语义、忽略文件冲突中止、切换分支拒绝、派发收据幂等与冲突、交付观察记录幂等、所有权读取（释放后不再是持有者并报告实际持有者）、计划绑定的主机证据检查，以及“读取失败不得写入”的注入测试（伪造 `git` 使 `worktree list` 或 `status` 失败，断言没有调用 `update-ref`/`merge` 且目标不变）。
  - 交付后重新发现检出：`RefOnly` 计划之后新建的 linked checkout 若索引/工作树仍是旧内容（例如 `D  reviewed.txt`）必须失败（父评审反例）；干净且 HEAD 等于批准提交的新检出可以成功；真正未检出的目标仍可通过；锁定、重复（fixture 用 `worktree add --force` 制造）、外来仓库、计划中的检出被删除、移动或切换到别的分支都给出明确原因而不是虚构成功；两个仓库其中一个的当前检出失败时保留另一个已验证结果。
- `cargo test --locked -p deepseek-code-core --lib super_agent::integration::tests`、`super_agent::state::tests`、`super_agent::store::tests`、`super_agent::launcher::tests`、`super_agent::orchestrator::tests`、`super_agent::crp::tests`：9/15/23/13/16/7 项，5A 准备、状态门禁、存储、启动和编排行为不变。
- `cargo test --locked -p deepseek-code-core --lib native_engine::workspace::tests`、`native_engine::tool_loop::shell_cwd_tests`、`native_engine::child_session::tests`：12/9/7 项，第 13 轮的 shell 缓存修复保持。
- `cargo test --locked -p deepseek-code-cli --test super_agent_review_loop`：3 项旧场景（默认关闭交付）仍在 `DELIVERY_PENDING`/`PREPARED` 停止，目标分支不变。
- 未验证：真正快进/`update-ref` 目标分支和 Coder 交付指令属于 5B2；协作式 `branch:` 锁不能阻止任意外部 `git` 命令；进程崩溃与数据库之间的原子性未承诺；Windows 未运行。证据日志在开发支持目录 `round15-evidence/`，每次尝试使用独立文件名。已成功持久化并显式释放的交付计划目前无法用同一证据重新取得所有权（通用 `TaskStore::try_lock` 的 `Released` 语义），该限制保留到 5B2 处理。

### Super Agent 第 5B2a 阶段：单仓库交付经同一 Coder 执行

交付默认关闭，只有 `settings.json` 的 `superAgent.delivery.enabled=true` 才启用，因此 0–5A/5B1 的既有流程和三个真实 CLI 场景保持原行为。启用后，处于 `DELIVERY_PENDING` 且有一个或多个 Git 仓库、没有非 Git 目标的任务会把主机验证过的集成提交逐个交给**原 Coder 会话**落地：已检出的目标分支在该真实工作树执行 `git merge --ff-only --no-overwrite-ignore`，未检出的目标执行带旧值的 `git update-ref` CAS。命令在派发前由主机按持久化计划生成，路径用单引号引用；每个 Git 读操作都必须成功，`worktree list`/`status` 读取失败时绝不继续写入。主机只负责调度与验证，真正写操作由 Coder 完成，且从不 push。

- 派发前持久化 `sa_delivery_dispatches` 收据（task/spec/round、计划 operation、Coder 会话、purpose，随后绑定 job）。重复 tick 复用同一计划、收据和启动操作，不会重复派发；收据事实冲突会明确失败。
- 已持久化的计划是权威：主机不再用移动后的目标重算基线。每次先检查计划仍绑定当前 `INTEGRATION_PREPARED`/`REVIEW_COMPLETED` 证据和 reviewed/integrated 提交，再检查 `branch:` 锁仍由本任务和本次 lock operation 持有；释放或被其他任务持有则阻塞，不派发、不写 Git、不窃取锁。
- 第 33/34 轮把交付执行身份结构化：`DeliveryOperationIdentity`（版本、task/spec/round、计划 operation、证据/审核事件、内容指纹、repository、old/new OID、attempt、原 Coder 会话）决定唯一 purpose key（`delivery.v<N>.<sha256>`）；同一计划下 A/B 仓库或不同 attempt 得到不同 operation key 与 job，单独的 legacy `delivery` 只用于旧记录。身份随 operation、job 目录中的 `delivery-identity.json` sidecar、handle 和 receipt 传递；接管或观察前逐项校验。typed 记录缺少必需版本或字段会阻塞，不会静默降级为旧格式；sidecar 以原子 no-clobber 方式写入。
- 旧 `purpose="delivery"` 记录只在完整持久化单仓库计划唯一绑定、且原 job 存在时才被接管；不升级为新记录、不猜测仓库、不另起子进程。
- 第 35 轮实现顺序多仓库交付：完整持久化计划仍按 task/spec/round/证据校验仓库全集，不构造单仓库替身。每个 tick 按计划持久化顺序只推进一个尚未完成的仓库；同一 Coder 会话、每个仓库各自 typed purpose/job，不会并发恢复同一 Coder。已到达 approved new OID 的仓库只记录一次观察，不运行命令；待交付仓库在派发前重新读取该仓库的 old OID、ref、检出拓扑、干净状态和协作锁，历史观察不是新副作用授权。
- 每个仓库的子进程停止后只验证该仓库并独立持久化观察；A 已交付而 B 仍是旧提交不算失败。B 失败时阻塞原因点名仓库，保留 A 的观察、真实提交和进度，不回滚、不提前产生 `INTEGRATION_COMPLETED`。只有全部计划仓库都有匹配的持久观察、且再次实时验证所有目标仍等于 approved new OID 后，才记录任务级完成；已观察仓库之后发生漂移会阻塞而不是刷新旧基线。失败仓库保持阻塞并保留原 attempt，不自动发明重试。
- 有 job 收据时只做只读观察：运行中的子进程即使目标暂时匹配也不会被判定为完成；子进程停止后才按固定计划验证真实 ref 和当前检出。读取 job 时必须先验证实际文件的 job id、Coder session 和 Router parent，身份不符或状态不可读会明确阻塞并保留文件，不能把该 job 的状态当作交付证据。
- 收据存在但尚未绑定 job 时，宿主读取 launcher 的持久化 operation；其内容必须先通过 task/spec/round/purpose、Coder role/resume kind 和原 Coder session 校验，之后才允许接管已保留或运行中的 job、绑定收据或观察状态。仅仅命中相同存储 key 不构成证据；矛盾记录、畸形 job ID、读取错误和不可解析状态都产生明确阻塞原因，原记录和文件保持不变，不会新起子进程或覆盖收据。
- 只有主机验证通过才在同一事务内把仓库标为 `COMPLETED` 并追加 `INTEGRATION_COMPLETED`（`complete:true`），任务进入 `FINALIZING`。失败、脏检出、错误 OID 或成员伪造的成功消息都保持未交付并阻塞。
- 第 37 轮补齐完成路径：`FINALIZING` 任务只通过既有事务门禁记录 `TASK_COMPLETED`，且需同时满足：持久不可变交付计划仍绑定当前 task/spec/round 及其准备/批准事件、仓库集合精确且唯一、所有目标再次实时验证仍等于 approved new OID、每个仓库都有匹配的持久观察、当前轮次存在可解码的 `FINAL_DELIVERY` Review 与 Harness、无仍在运行的 Coder/Reviewer 子进程，以及主机记录过对那份 harness 的分析事件（该事件只在存在真实、当前、非阻塞且被决定事件引用的 harness 时写入）。任一条件不满足即保留具体原因并等待或阻塞，绝不直接改状态、不伪造报告。
- 内容未改变的集成沿用实现阶段批准进入交付，因此交付后由原 Reviewer 会话以独立 purpose `final-completion` 再提交一次真实 `FINAL_DELIVERY` Review+Harness（不复制、不改写实现报告）；内容改变时复用交付前的 `FINAL_DELIVERY` 审核。两个阶段 operation/job 互异。`CrpSubmit` 在 `FINALIZING` 只接受当前任务 Reviewer 的当前 spec/round 的 `FINAL_DELIVERY` Review 与 Harness，拒绝 `IMPLEMENTATION`、过期 round 和非成员提交；真正的工具拒绝会导致明确阻塞而不是无限等待。
- 完成记录只写一次（确定性 operation），额外 tick 与重开数据库不会重复事件或重放交付；本增量不产生 `CLEANUP_COMPLETED`，也不清理工作树。非 Git-only 任务即使处于 `FINALIZING` 也不会被该 Git 完成器标记完成，其快照完成规则留待后续增量。
- 多个 Git 仓库按上述顺序交付；Git+非 Git 混合目标仍明确不派发，任务留在 `DELIVERY_PENDING` 并在 tick 报告中给出警告。已观察 A 之后，待交付 B 若检出变脏、目标 ref 漂移或协作锁被释放/被其他任务持有，编排层在启动任何子进程前以点名 B 的原因阻塞：A 的观察与真实 ref 不变、持久不可变计划的 old/new OID 和证据不变、不刷新基线、不重新获取或窃取锁、不产生全局完成事件。
- `cargo test --locked -p deepseek-code-cli --test super_agent_review_loop`：21 项。除原有场景外还有：未检出目标 CAS 成功；带空格路径的 linked checkout 原地快进成功；交付命令失败但成员声称成功时不交付并阻塞；计划已释放且他人持锁时不派发；持久化 `deny Bash` 规则下交付子进程收到真实 `Permission denied: Explicit tool deny rule`；交付前通过 `session_permission_mode` 把 Router 父会话改为 `DontAsk`（Orchestrator 仍以 `bypass` 构造，且不写 Coder 专属 deny）时，恢复的原 Coder 在 Bash 调用上收到真实 `Permission denied: dontAsk mode: ...`，目标、索引与文件逐字节不变、无完成事件、Coder 身份不变；Git 已被移动但宿主尚未观察时重开新 context/store/orchestrator 不重复派发、不在子进程运行时完成、停止后只观察一次；launcher `after-spawn` 崩溃边界（operation 已保留、job 已运行、handle 未写入、receipt 无 job）由新宿主按保留的 job 身份接管，不按旧基线重新校验、不重启子进程；同一崩溃边界下把 operation 的 `task_id` 改成另一个任务（仍写在其正确 key 下）时，新宿主在绑定收据或观察前明确阻塞，零重启、零重复命令、目标保持子进程真实写入后的提交、矛盾记录原样保留；当前 typed 身份的有效 receipt 已保存但从未启动、随后目标被外部移动时，恢复必须走到实时 drift 校验并以 `moved`/`approved plan is stale` 阻塞，零子进程、目标保持外部移动后的提交、无完成事件；已绑定的 receipt 被改指向同一 Coder 的另一个真实 job 时，新宿主在观察前阻塞，收据与 operation 原样保留（不覆盖为原 job 或其他 job）、零重启、零重复命令、目标保持子进程真实写入后的提交、无完成事件。真实集成报告由显式 fixture gate 持有：任务仍为 `INTEGRATING` 时重复 tick 必须复用同一 integration job/operation 且命令次数不变，释放后才允许进入最终审核。每个成功场景验证同一 Coder 会话、真实 Bash 工作目录、目标提交、逐字节内容、交付命令只执行一次、重复 tick 与重开数据库不重复派发。另有两个真实双仓库场景：两个独立 Git 仓库、不同目标分支、含空格路径；每个仓库由真实 `dcl` 子进程执行一次 Bash 交付命令，fixture 拥有的执行日志记录命令实际运行次数，durable `tool_calls` 记录两次 Bash 调用且 invocation ID 不同（provider 在两轮复用同一个 raw `delivery-bash` ID）。成功场景在第二个仓库子进程仍运行时确认没有任务级完成事件，释放后才最终完成；A 成功/B 失败场景保留 A 的观察与提交、点名 B 阻塞，重开宿主后不重放 A、不新增子进程。第三个双仓库场景覆盖“真实写入后、观察前”的宿主重开：B 子进程已执行真实 Git 命令但仍被模型完成门挡住时，丢弃旧 host/context，从同一持久化 DB 重开新 host 并 tick，必须接管同一 typed job/operation、零新增子进程、零 `INTEGRATION_COMPLETED`；释放后新 host 才观察到成功。最终完成后再额外 tick 和再次重开仍只有一个 `INTEGRATION_COMPLETED`，且不产生 `TASK_COMPLETED`/清理；A/B 执行标记各一次、durable invocation 仍为两个不同 ID。第 37 轮另加两个真实完成场景：内容未改变时，同一 Reviewer 被独立 purpose 再次恢复并提交真实的 `FINAL_DELIVERY` Review 与 Harness（工具结果确认两次 `Report recorded`），主机在只有 Review 时必须等待 Harness；内容改变时复用交付前审核、不新增子进程；两者最后都通过事务门禁进入 `COMPLETED`，只产生一个 `TASK_COMPLETED`、无清理事件，额外 tick 与重开 DB 不重复。这些是真实单/双仓库实际交付；`crates/core` 的 `two_repository_deliveries_get_distinct_operations_and_jobs` 使用 fixture shell 子进程，只证明 A/B operation 与 job 互异，不能替代真实 CLI 证据。第 45 轮另加两个真实完成门禁场景（两者均仅 Linux 运行）：最终 `FINAL_DELIVERY` Review 与 Harness 已落库、真实子 Reviewer 仍停在 held 模型响应时，删除该 job 的 `state.json` 后必须阻塞且原因点名该 job，重开宿主仍阻塞、不新建替代 Reviewer、损坏证据保留；对照场景在报告齐全但 job 仍 running 时等待，释放 held 响应、真实子进程正常退出后才恰好产生一次 `TASK_COMPLETED`。就绪由数据库记录、HTTP mock step3 记录和 job 状态屏障证明。负例收尾不复用要求所有 job 成功的通用等待：先恢复被暂停的 supervisor，等它重新发布 `state.json`（避免子进程最后读取控制目录时与删除竞态），再释放响应；该故意损坏的 job 允许真实终态 `failed`/`cancelled`（缺失 state 不是退出证明），其他 job 仍要求 `completed`；子进程与 supervisor 的停止分别按记录的 PID+start 验证，sentinel 必须存活。
- 预取观察者：`cargo test --locked -p deepseek-code-core --lib prefetch::`：2 项。`git status` 预取在独立任务中执行且可能与同一工作树的 `git add/commit` 重叠；观察者必须使用 `GIT_OPTIONAL_LOCKS=0`，否则 Git 会刷新并重写索引（短暂持有 `index.lock`），导致写入方报 `index.lock: File exists`。测试用“内容相同但 mtime 变化”使 stat 缓存失效：对照组证明普通 `git status` 会改写索引，观察者组要求索引字节完全不变且状态仍被缓存；负向对照（去掉环境变量）失败，修复后通过。
- 未验证：失败仓库的受控重试协议、完成后的清理/工作树/分支归置、Watcher/UI、Git+非 Git 混合交付、非 Git-only 完成；上述“实时父策略”证明引擎按持久化父模式拒绝 Bash，不是操作系统级沙箱拒绝；外部 `git` 与协作锁之间的竞态仍不具原子性；Windows 未运行。
