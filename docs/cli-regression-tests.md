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

团队请求附带记忆召回的回归由 `cargo test --locked -p deepseek-code-cli --test teams` 覆盖。夹具用 user 消息首行的完整任务标记识别队员，路由和计数共用该判断。`child_request_marker_survives_appended_context` 覆盖纯文本和附加召回内容的文本及分块消息，也验证 leader 消息在其他角色、后续行或更长首行中提到任务时仍归为 leader。独立 CLI 用例继续验证队员启动、消息唤醒及团队删除。

准入交棒的自动回归在 `--bin dcl` 中（暂停 Tokio 时钟，不依赖墙钟）：`persistent_worker::admission_tests` 覆盖“只有确实已完成的回合才等待”“超过旧 250ms 轮询窗口仍在等待”“宽限到期按未提交拒绝且不释放他人准入、之后的真实释放才准入”“空闲 host 立即准入”。真实协议路径由 `--test session_worker` 的 `turns_submitted_right_after_completion_are_admitted` 覆盖（连续三个回合紧跟完成提交，各只请求一次）。

若出现 `Pass its exact name in the ToolLoad names array`，先确认场景是否需要显式加载工具或增加模型加载步骤；不要把未执行工具导致的后续空结果误判为 Worker 或消息传递失效。

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

- 每轮用新的数据目录。启动前安排清理，正常退出、断言失败及超时都要终止并等待回收自己启动的 dcl。关闭 PTY 或停止 tmux 后仍要检查进程，必要时按已确认的进程身份强制结束并回收。
- `until` 等待的文字如果也出现在自己输入的提示词里，会立刻匹配成功，这时后面的输入会变成运行中发送。等待模型回复时要选提示词里没有的文字，或用固定等待。
- 不要用 `pkill -f "某段文字"`，它会连当前 shell 一起结束。
- 用外部 `kill` 命令结束进程组时，负数 PID 前必须加 `--`（`kill -9 -- -123456`）。Debian 的 procps `kill -9 -123456` 会被解析成 `kill(-1, SIGKILL)`，结束当前用户的全部进程，包括 tmux 和正在运行的 dcl。代码里优先直接调用 `rustix::process::kill_process_group`；`external_kill_never_takes_a_negative_pid_without_separator` 会检查源码。
- 怀疑测试会结束无关进程时，用 `unshare --map-current-user -Upf --mount-proc` 在独立 PID 命名空间里运行，信号到不了命名空间外。
- `--resume` 时如果 worker 还活着，不能带 `--permission-mode`，这是有意的限制。

## 场景

### I50：父轮次转入后台后的子代理树

- `cargo test --locked -p deepseek-code-cli --bin dcl -- idle_panel_`：同一父会话下的前台、后台子代理各占一行；没有独立后台任务的前台子代理也按子会话身份区分，重复的启动事件不增加行，失去执行租约后移除对应行。
- 在真实终端用本地模拟模型一次返回两个 `Agent` 调用（同类型，描述分别为 A、B），分别阻塞两个子请求。两个后台调用、或先后台后前台时，应同时看到 A、B；依次放行后应只剩 B、再无子行。混合模式下另起一轮，在两个请求均阻塞时按 Ctrl+B 返回输入框，重复检查这三个状态。
- 两个前台调用即便在同一模型响应中也会串行执行：先仅有 A，放行 A 后仅有 B。再测一次在 A 运行时按 Ctrl+B，返回输入框后仍应依次看到 A、B，而不是空树。用实际抵达模拟端点的请求核对屏幕，不把两个调用声明当作并发执行。

### 文件锁的平台一致性

共用引擎的文件锁通过 `fs::try_lock_file`、`fs::lock_file` 获取，短暂竞争使用 `fs::try_lock_briefly`，逻辑结束时用 `fs::unlock_file` 显式释放。Android 使用系统 `flock`，其他平台沿用标准库。

1. `cargo test --locked -p deepseek-code-core --lib fs::file_lock`：首次获取成功，第二个独立句柄返回 `WouldBlock`，失败的探测不会释放原锁，持有者释放后可以重新获取。`an_inherited_descriptor_keeps_a_close_only_lock_but_explicit_unlock_frees_it` 用真实子进程继承描述符证明：只关闭持有者不会立即释放（`flock` 属于打开文件描述），显式解锁才会。该区别存在后，进程内其他线程 `fork` 期间“关闭即释放”不再保证即时，因此原有测试改为通过生产路径显式解锁；排他性断言保持不变。
2. 在 Android 真机运行同一锁测试，并用首次创建的会话发消息。真实存在的竞争者仍被拒绝，正常会话可以开始问答。
3. 回归会话执行租约、presence、一次性 hook、设置写入和 worker 恢复测试，确认调用方统一使用共用锁入口；不要将不支持的锁调用等同于已有执行者。`services::session_worker::recovery` 的 `recovery_releases_the_startup_lock_for_a_retained_descriptor` 和 `a_failed_recovery_releases_the_lifetime_locks_it_took` 覆盖启动/恢复锁在成功和失败路径上的显式释放，包括保留描述符的情况，并且不释放其他进程持有的锁。

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

### A3. 提交提示词时输入框交接无抖动

修改 `crates/cli/src/terminal/editor.rs`、`crates/cli/src/terminal/live.rs`、`crates/cli/src/session.rs` 或 `crates/cli/src/worker_frontend.rs` 的提交/交接渲染逻辑后执行：

```bash
cargo test --locked -p deepseek-code-cli --bin dcl -- submit_handoff idle_tip_above_input live_first_frame
cargo test --locked -p deepseek-code-cli --test prompt_submit_stability -- --nocapture
```

1. 36×100 真实 PTY/ConPTY 首轮打开时，Tip 位于输入框上方而不是 footer 下方；底部的上边框、输入行、下边框、footer 固定占据最后 4 行（`rows - 4 .. rows`）。
2. 首轮按 `Enter` 提交后（模型响应门控延迟期间），不擦除底部输入框，不产生一行下移；工作态状态行直接出现在输入框上方，且带有初始模型标签。
3. 首轮流式回复完成并交棒回空闲态、以及第二轮提交并在请求门控期按 `Esc` 取消期间，底部输入框边框与 footer 始终停留在同一组屏幕行，且请求期 `Esc` 可正常中断并返回空闲态。
4. 满屏状态下提交与接管不会把旧输入框/旧 footer 滚进 transcript。

### A4. 斜杠补全菜单输入不闪烁、不残留

修改 `crates/cli/src/terminal/editor.rs` 的 `CommandOverlay` 或空闲输入事件循环后执行：

```bash
cargo test --locked -p deepseek-code-cli --bin dcl -- slash_menu_overlay startup_banner_and_slash_menu
cargo test --locked -p deepseek-code-cli --test prompt_submit_stability -- --nocapture
```

1. 对话历史（含已提交提示词的白色 `────` 分隔线）紧邻空闲输入框上方时，输入 `/` 弹出 5 行命令补全菜单，再继续输入 `c`、`o`、`x` 与退格：
   - 保持 5 行菜单期间，普通编辑/导航按键不先清空菜单再重画，中间帧不得闪出菜单底层的 `────` 分隔线或空白（普通视图与 `Alt+M` 紧凑视图均为单帧合成）；
   - 匹配行数收缩（如输入 `/cox` 变为 1 行 `/context`、`/coxz` 变为 `No commands match`）时，仅恢复露出的背景行，仍被菜单覆盖的行原地更新且隐藏光标；
   - 按单次 `Esc` 可直接关闭菜单或 `No commands match` 提示（保留草稿），提交 `Enter`、输入框增高触发滚屏或缩放/鼠标操作前菜单干净清除，历史与滚动缓冲中不残留任何菜单行；
   - 取消回合后 `hand_off_output` 若历史已以空行结尾不再多预留 1 行，交接回空闲态时历史不向下跳动。

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

### C2. 紧凑工具组与复制不跳动

1. 用 `runs/ux/drive.py` 和开发目录 `.venv/bin/python` 在独立数据目录驱动真实 PTY。分别直连和独立 tmux 运行 `/rename control`、`/copy-session-id`，保存 Enter 前后原始字节与快照。复制反馈在原输入框状态行；没有擦除到屏幕末尾、切换输入模式或重画边框。OSC 52 只传剪贴板数据，不能包含来自文本的光标/清屏控制。tmux 的 `show-buffer` 必须为会话 ID。运行中复制仍需保留当前工具和输入区。
2. 本地模型 fixture 运行 Bash、Glob、失败 Read，Alt+M 摘要应显示 shell/read/patterns 和失败数。点摘要展开，点组顶部 `click to collapse` 收起；组内工具仍可独立切换短结果/完整结果。Alt+M 关闭再打开恢复摘要；普通视图和 Ctrl+O 不受影响。恢复会话后重复同一检查。
3. `cargo test --locked -p deepseek-code-cli --bin dcl terminal::scrollback` 覆盖真实工具名分类（包括 PowerShell 和全部编辑工具）、同名计数、宽度截断/剩余调用数、失败数、组级状态、模式重开和 reasoning 不算工具。自定义标题包含 Bash/Read 文本不能改变工具分类。
4. 本地 mock 返回未知名称的 `ToolLoad` 调用（例如 12 个 `✈️`），确认结果记为失败。恢复会话，在 64 列打开紧凑模式并调整到 32 列。CLI 应继续运行，摘要按终端列宽保留完整字素和右括号。在 `terminal::tool_view`、`terminal::scrollback`、`terminal::progress` 中回归 CJK、变体选择符、ZWJ emoji 和组合字符，包括极窄列宽。

### C3. 拖动选择与右击复制（SSH/tmux）

1. 在提示符、模型输出和 `Ctrl+O` 记录里按住左键拖动：出现反色选择，松开后高亮保留，输入框上方内容区右下角显示 `按 c 复制`（英文界面为 `Press c to copy`）；在保留的选择上右击或按 `c`，所选文本进入剪贴板，高亮消失，同一位置显示 `已复制 N 个字符`（英文为 `Copied N characters`），且 `c` 不会插入草稿。提示约 2 秒后自动消失，按任意键也会立即消失；底部状态栏保持默认。提示行不得出现反色或其他背景色。没有选区时再次右击不复制、不报错，按 `c` 正常输入。
2. 复制失败的路径在内容区显示 `复制失败`，选择同样取消；滚轮、左键点击展开、单双击状态行复制保持原行为。
3. 本地桌面终端：右击后粘贴应得到原文，包含中文和换行；本地会话仍优先使用系统剪贴板。
4. 在 tmux 中运行（含 SSH 远程主机）：tmux 需已开启 `set -g mouse on`，应用才能收到鼠标事件；拖动选择后右击，`tmux show-buffer` 应为所选文本，屏幕上不再保留反色高亮，内容区右下角显示 `已复制 N 个字符`。外层终端支持 OSC 52 时，`load-buffer -w` 也应把同一文本送到本地剪贴板。即使远端存在 `pbcopy`/`xclip`，也不能只写远端剪贴板。
5. 运行中的反馈还要单独验证：模型流式输出时拖动选择并右击，内容区右下角提示出现且流式输出继续；等待约 2 秒确认提示自动消失。分别用 `LC_ALL=zh_CN.UTF-8` 和 `en_US.UTF-8`（或 `config.json` 的 `language`）启动，确认中英文提示都正确，且提示行没有背景色。`Ctrl+O` 全页记录和文件差异页仍在各自页脚显示 `Copied N characters to clipboard`。
6. 自动回归（分别执行）：

   ```bash
   cargo test --locked -p deepseek-code-cli --bin dcl terminal::mouse_selection::tests::
   cargo test --locked -p deepseek-code-cli --bin dcl terminal::file_review
   cargo test --locked -p deepseek-code-cli --bin dcl terminal::transcript_pager
   cargo test --locked -p deepseek-code-cli --bin dcl editor_keeps_right_click_events_for_selection_copy
   cargo test --locked -p deepseek-code-cli --bin dcl extended_commands::tests::clipboard
   ```

7. 平台说明：Windows ConPTY 的右击由 crossterm 鼠标事件驱动；macOS/Linux 的 SSH+tmux 需要真实终端复验，不能用本机单测替代。

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

启动/恢复锁在逻辑结束时显式解锁（`flock` 属于打开文件描述，fork 出的副本会推迟 close 释放），因此恢复返回后可以立即启动新的 worker。只有取得锁的进程（记录取得时的 PID）才会解锁，子进程不会误放父进程仍持有的锁；同一原因，`fs::file_lock` 中“关闭即释放”的旧断言在会 `fork` 的测试进程里并不可靠，现已改为通过生产路径显式解锁，关闭与显式解锁的确定性对比由 `an_inherited_descriptor_keeps_a_close_only_lock_but_explicit_unlock_frees_it` 覆盖。

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

#### D9. 占用时的只读观察与自动唤醒门禁

另一个进程持有执行租约时，恢复的 REPL 必须保持只读，并且所有会自动开回合的入口都必须先检查占用。

1. **后台任务占用**：`/resume` 显示 `✻ Task … is executing this session since …`，普通消息提示未提交；`/continue` 接回任务，`/tasks attach ID` 打开完整视图，`Esc` 退出。
2. **直接执行占用**：显示 `A turn is still running in another process since …`，继续显示已发布的文本和工具行；不得出现原始的 `lock acquisition failed`。
3. **自动入口**：占用期间 task notification、`/loop`、cron、hook wake、remote deferred prompt 都不得创建新 turn；通知留在队列中，占用结束后也不自动提交，直到用户显式输入或 `/continue`。`dispatch_input` 自身也必须再次检查，不能消费队列项。
4. **恢复输入**：owner 结束后显示 `The other turn finished; input is enabled`，只读 banner 不再出现；之后输入的消息可以提交。
5. **goal proposal 失主**：另一个 frontend 接管 goal proposal 时，任务 view 不退出、不 detach；日志记录一次，重新 acquire 一次，失败则提示 `Goal proposal status unavailable; task output continues.`，文本和工具行继续。
6. **切换会话清理**：`/resume`、`/branch`、`/clear` 后，Remote Control 断开并提示，旧 `/loop`、草稿、`NEXT_DRAFT` 和排队提示被丢弃；新会话不会收到旧 remote 注册的消息或权限。

自动回归：

```bash
cargo test --locked -p deepseek-code-cli --bin dcl notifications_and_direct_input_wait
cargo test --locked -p deepseek-code-cli --bin dcl reset_for_session_change
cargo test --locked -p deepseek-code-cli --bin dcl a_second_top_level_turn_is_rejected
cargo test --locked -p deepseek-code-cli --bin dcl losing_the_goal_proposal_frontend
cargo test --locked -p deepseek-code-cli --bin dcl a_caller_banner_suppresses
cargo test --locked -p deepseek-code-cli --bin dcl turn_command_tests
cargo test --locked -p deepseek-code-core --lib current_rejects_a_worker_record
```

第 1、2、4 条及 banner 的终端渲染需要真实 pty。2026-11-26 已在 macOS 用 tmux + 本地 mock SSE 跑通：owner 为 `dcl --resume ID --print`，observer 在 40×140 tmux 面板中 `dcl --resume ID`；banner、已发布文本、未提交提示、owner 结束后的输入恢复均通过，且观察期间没有模型请求。脚本和日志见 `dev-support/deepseek-code/session-occupancy-20261126/`（`dcl_occupancy_tmux.py`、`pty-run.log`）。Windows ConPTY 尚未运行。第 5 条由 `worker_frontend::failure_tests::losing_the_goal_proposal_frontend_keeps_the_task_view_and_its_output` 覆盖：running job 的 proposal presence 被另一 live frontend 接管后，观察只记录失败、禁用提案 UI，任务完成且无 cancel/detach。banner 去重由 `observe::tests::a_caller_banner_suppresses_the_first_running_notice` 覆盖；`/loop stop-*` 边界由 `turn_command_tests` 覆盖。`read_prompt` 期间被另一进程接管的 tick 门禁尚未用真实 pty 重跑。

#### D10. 已删除目录的只读恢复

保存的工程目录或绑定的会话 worktree 被删除后，历史仍在 `global.db` 与 `data/sessions/<id>/session.db`。交互式 REPL 必须能只读打开，严格执行路径仍必须失败。

1. **严格路径不变**：`dcl --print --resume <id> …`、`--input-format stream-json`、`--fork-session`、`--init-only`、SDK、worker、Desktop 和 `--continue` 仍返回缺失目录错误（工程目录为 `Folder must be an existing absolute directory` / `Project folder is unavailable`，绑定 worktree 为 `Conversation worktree is unavailable; restore it before continuing`）。不得把这些入口降级为只读。
2. **交互式 `dcl --resume <id>`**：不改变启动 cwd，不写目标会话库，不设置权限模式，不运行 SessionStart/Setup hooks，不注册协调行；显示 banner、完整历史与 `✻ Read-only mode: directory no longer exists (<path>).`。普通提示、多行提示、skill 提示、remote 提示、`!命令` 与 `# 记忆` 都提示未提交；`Shift+Tab` 与 effort 键提示只读；`/history`、`/export`、`/copy`、`/context`、`/cost`、`/status`、`/sessions`、`/tasks list|show|sessions|records`、主题/颜色/输出样式的无参数查看、`/help`、`/exit` 可用；`/clear`、`/branch`、`/compact`、`/model`、`/effort`、`/permissions`、`/rename`、`/config`、`/tasks cancel|answer|stop-session|attach` 等起始回合或修改会话/工作区的命令被拒绝。
3. **切换**：在有效会话中输入 `/resume <缺失会话>` 进入只读；缺失目录不得显示为可写。`/resume` 选择器按 `Ctrl+A` 显示所有项目时，缺失目录的详情行带 ` · [missing]`。再次 `/resume <有效会话>` 恢复可写、权限模式和宿主 lifecycle hooks；重建缺失目录后 `/resume <原会话>` 也恢复可写。
4. **保存的 provider/model 已从 `config.json` 删除**：仍以 observation-only 的显示绑定打开，状态行和 `/status` 显示保存的 provider/model 与 `read-only (directory missing)`，`/status` 不探测 worker、工具或语言服务器，状态行外部脚本不执行。
5. **数据安全**：只读打开前后目标会话的 `updated_at`、`message_count` 与权限模式行不变；`turn()`、`clear()`、`branch_saved()`、`set_permission_mode()`、`cycle_permission_mode()` 均被门禁拒绝；`sync_workspace()` 无操作。信任、归档、非 Code、现有目录 canonical 路径变化等错误仍与严格 resume 一致，缺失目录不是信任绕过。

自动回归：

```bash
cargo test --locked -p deepseek-code-core --lib inspection_tests
cargo test --locked -p deepseek-code-cli --bin dcl missing_workspace_tests
cargo test --locked -p deepseek-code-cli --bin dcl resume_tests
```

`inspection_tests` 覆盖缺失工程目录、缺失绑定 worktree、未信任、现有目录路径漂移、归档会话/工程、非 Code 会话与 projectless 私有目录；`missing_workspace_tests` 覆盖有效→缺失→有效、重建目录、缺失 provider、状态/模型标签、命令白名单与只读 footer；`resume_tests` 覆盖 `[missing]` 标记。macOS 默认 soft fd limit（256）会让 core 全量测试出现 `Too many open files`，先 `ulimit -n 8192`；该全量套件另有与本次改动无关的并行 cwd/超时失败，单独重跑通过。真实交互需要隔离数据目录与 PTY：先创建一个有历史的会话并删除其目录，再在另一目录运行 `dcl --resume <id>` 或 `/resume <id>`，确认只读 banner、历史、拒绝输入、`/history`/`/status`、`/resume <有效>` 恢复；在 `--resume` 选择器中按 `Ctrl+A` 选择带 `[missing]` 的条目。Windows ConPTY、Desktop WebView 和真实 provider 未在本轮验证。

### F. /tasks

有两个后台命令时输入 `/tasks`：

1. 显示 `Background`、`2 active shells`，每个任务一行（`● 命令  running`），最新的在前。
2. `↑`/`↓` 选择，`Enter` 看详情，详情里 `←` 回到列表并保持选中项。
3. 列表和详情里按 `x` 停止：任务立即从列表消失；停止最后一个后列表自动关闭，不再输出其他内容。
4. 没有任务时输出 `No background tasks.`。`/tasks completed`、`/tasks show ID`、`/tasks attach ID` 文本命令照常可用。

#### I7. Monitor 自动续期

```bash
cargo test --locked -p deepseek-code-core --lib tools::monitor::tests::
cargo test --locked -p deepseek-code-core --lib task_cleanup
cargo test --locked -p deepseek-code-core --lib native_engine::tool_loop::tool_loading_tests::monitor_is_deferred_and_loadable
cargo test --locked -p deepseek-code-cli --bin dcl -- monitor_events_are_queued_once_and_announced idle_session_wakes_for_a_monitor_event
```

1. 省略 `max_runtime_ms` 时，脚本仍在运行也会在首个 `timeout_ms` 到期后结束，最终事件为 `expired`。`max_runtime_ms` 必须是大于 `timeout_ms` 且不超过 86400000 的整数。
2. 启动会输出 PID 并等待本地文件的脚本，收到 `renewed` 后用 `TaskGet` 确认同一任务仍为 `running`。创建文件后，第二条输出的 PID 与首条相同，随后仅收到一个 `exited`。
3. 在收到首次续期通知后调用同一任务的 `TaskStop`，确认任务变为 `cancelled`，最终事件为 `stopped`。
4. 设置 2 秒间隔和 5 秒总上限。用命名管道让本地脚本等待放行，确认首条输出已写入事件后再推进暂停的 Tokio 时钟。验证两个续期边界以及最后一个间隔缩短至总上限。将时钟保持在上限，用 Linux pidfd 单独确认进程退出，再检查最终任务记录和 `expired` 事件。短间隔一分钟内只发一条 `renewed` 通知，重复收取事件应为空。
5. 在真实终端用本地模型 mock 通过 `ToolSearch` 加载 `Monitor`，核对请求中的新 schema。让脚本保持运行直到收到续期通知，确认终端显示该通知，模型恰好收到一条新的续期消息。让脚本退出，再检查模型没有收到重复续期通知。
6. Linux 子代理用本地模型 mock 加载并启动可续期的 Monitor。先等待续期并获取脚本和后代进程的 pidfd，再分别放行子代理正常返回和取消父执行 future。两种情况下任务都进入 `cancelled`，只产生一条 `stopped`，两个 pidfd 都确认原进程已退出，`waitid` 确认直接子进程已回收。正常返回还向父会话报告已停止的任务。
7. 省略总上限时，在当前线程的 Tokio 运行时第一次调度 Monitor 前把时钟推进 60 秒。它仍有完整的 30 秒间隔，观察期内继续运行。计时器赢得 select 时仍直接走原有的 `expired` 分支。
8. 子代理的 Monitor 续期后，通过其会话的普通 `TaskUpdate` 尝试完成或删除任务、清除 `metadata.monitor`、改写 `metadata.log_dir`，四项均应被拒绝。说明和普通 metadata 仍可修改。放行子代理返回，确认脚本和后代都退出、直接子进程已回收、任务为 `cancelled` 且仅一条 `stopped`。普通 `TaskCreate` 和 `TaskUpdate` 也不能伪造 Monitor 标记。
9. 遍历 Monitor 的任务状态表，分别使用普通 TaskUpdate 和进程所有者接口验证所有允许与禁止的状态写入。对三个终态分别测试再次启动、改写结果、重复停止、同态写入和删除；拒绝时同一请求中的说明修改也不能落盘。删除后不能复活原 ID。
10. 在隔离数据目录用普通文件阻止创建日志目录，另一次用不存在的命令模拟启动失败。两次均应能读到 failed 记录并删除。运行时在首次调度前退出也应使记录进入终态。
11. 子代理清理测试先等待脚本发布 PID，再查询已提交的任务库。用四个测试线程并行重复运行正常返回及 TaskUpdate 后返回的用例 20 次。
12. 锁住 SQLite 的 running 提交时，带总上限的脚本不能提前启动。发布成功后以启动脚本时的单调时钟计时；阻止 running 写入时进程的 pre_exec 标记应为空。
13. 脚本正常退出时保持 SQLite 写锁，确认只收到一次 exited。锁在 10 秒重试窗口内释放时，直接查询数据库应看到 completed 和退出结果；锁保持到写入方重试结束时，原记录应保持 running。再次读取和收取事件后仍应看到原记录。
14. 子代理的 Monitor 续期后将真实任务行置为历史状态 in_progress。放行子代理返回，验证父代理确认停止，shell 与后代均退出，且只有一个 stopped 事件。
15. 在上述写锁下归档日志目录，TaskGet 的 4 字节限制和截断标志应与当时已提交的 output 一致。用 1.5 秒后释放的写锁验证普通 TaskUpdate 的备注仍能写入且退出结果保留备注。另一次只将数据库改为只读后释放写锁，确认 TaskGet 和 TaskList 均可读取记录。
16. Monitor 正常退出后，用写锁挡住第一次终态写入，确保 TaskUpdate 已提交再放行重试。对比编辑回执、TaskGet、TaskList 与 SQLite：状态应为 completed，说明保留，updatedAt 取成功写入时的时间，不能早于已存时间（包括带时区偏移和纳秒的时间）。若编辑还设置了 output，终态摘要里的 task_output 应逐字保留该字符串，包括空串。
17. 重试合并 TaskUpdate 的 output 时，对照回执、TaskGet、TaskList 和 SQLite，逐字节核对内嵌 NUL、首尾控制字符、Unicode、转义文本及超过 1 MiB 的字符串。用预置的 JSON 行验证孤立代理项转义的原文，带 NUL 后缀的状态或时间戳不能按前缀匹配。通知的 subject 会过滤控制字符，但应保留 NUL 之后的正文。

### G. 长期记忆

1. `dcl memory add`、`list`、`list 关键词`、`update`、`delete` 输出 JSON；ID 不存在时报错。
2. `/memory` 菜单有 Long-term memories；列表可以搜索，`+ Add a memory` 能添加。
3. 详情里 `e` 编辑后列表立即更新并保留重要性，`x` 删除，`←` 返回，`Esc` 关闭。
4. `/memory` 的说明文件选项和 `/memory show` 不受影响。
5. `dcl config memory` 输出当前开关；`dcl config memory off` 后 `config show` 的 `memory.enabled` 为 `false`，CLI 与 Desktop 新请求的系统提示词不再包含 `<memory_guidance>`，MEMORY.md 索引和记忆目录写入同时停止；`dcl config memory on` 和 `dcl config set memory.enabled true/false` 作用于同一开关。`cargo test --locked -p deepseek-code-cli --test peripheral config_memory_switch` 覆盖这些分支。

提示词分段由 `cargo test --locked -p deepseek-code-core --lib prompt::` 覆盖：Code 与 Desktop 都按“身份行、`code.md`、`behavior.md`、`memory.md`”组装，记忆关闭时省略第四段，`cache_blocks` 同时识别四段和三段布局并在 Anthropic 请求中分别设置缓存断点。

#### 文件记忆（独立 core 模块）

修改 `crates/core/src/memory/files/` 后运行 `cargo test --locked -j 2 -p deepseek-code-core --lib memory::files::tests:: -- --nocapture`，并运行现有 `cargo test --locked -j 2 -p deepseek-code-core --test memory_transfer --test dream`、`cargo test --locked -j 2 -p deepseek-code-cli --test peripheral`。构建/测试命令使用任务指定的 run_checked.py 和共享 Cargo 队列；保留完整日志、真实退出码、实际测试名和数量。

- `scope_git_common_directory_shares_worktrees_and_subdirectories`：临时 Git 元数据夹具，只读 rev-parse；同仓库 worktree/子目录同 key，不同 clone 不同 key。`scope_non_git_multi_root_private_and_trust_are_explicit` 覆盖非 Git、多根去重/排序与 trust，`default_home_and_empty_workspace_use_persistent_private_owner` 覆盖默认 Home/空工作区及 owner 隔离；Unix 另测非 UTF-8 路径无 lossy 碰撞。
- `format_*` 与 `external_bad_files_are_retained_diagnosed_and_not_indexed`：中英文、四类/必填字段、Why/How、重复 YAML key、alias/tag、深度/大小/编码、凭据；坏文件留在磁盘且不进索引。
- `two_processes_racing_one_hash_yield_one_success_and_one_conflict`：两个真实子进程先读同一 hash，再由屏障同时放行，断言恰好 1 success/1 Conflict 和最终 generation。`cross_process_lock_times_out_and_cancellation_does_not_write` 另验真实进程锁和取消。
- `process_crashes_at_each_commit_boundary_recover_consistent_index_and_generation`：暂存、journal、正文、索引、catalog、generation、清理七个边界使真实子进程不运行析构直接退出，重开核对内容/索引/generation 和重复恢复幂等；另测删除恢复、外部冲突和 journal checksum 损坏不覆盖。
- `index_*` 与 `external_body_only_changes_invalidate_generation_without_rewriting_index`：索引自动修复/CAS/不能隐藏文件，200 行、25 KiB、4096 tokens 独立限额，整行截断与省略数；仅正文变化同样使 generation 失效。
- Unix 运行 `symlink_files_and_directory_swaps_never_follow_outside_targets`、`symlink_in_state_or_project_parent_is_rejected_before_creation`、`hardlinks_are_never_read_modified_or_deleted_by_the_store` 和路径穿越/设备名用例。
- Windows CI 预期运行同一测试过滤及 `windows_reparse_points_are_rejected_and_pinned_directories_cannot_be_swapped`、`windows_directory_reparse_point_is_not_a_managed_project_root`、`windows_junction_is_rejected_before_any_external_file_is_created`、`windows_canonical_path_case_aliases_have_one_scope`；符号链接用例要求 Developer Mode/创建符号链接权限，不能无声跳过。Linux 结果不构成 Windows 测试通过。
- P1 未切换旧 SQLite 后端，不把上述存储层测试当成模型权限、自动召回、CLI picker 或 Desktop/WebView2 验收；后续接入仍要执行本节原有 1–4 与 Desktop A。

#### M3 常驻索引（替代逐消息正文召回）

- 用独立临时 data-dir 和本地模型端点；给 global、team、project、subagent、session 的索引和正文各写不同标记。捕获主会话与子代理的实际请求：首条用户消息里先项目指令再按上述顺序放 scope/路径；主会话无 subagent，子代理无父 session，team 未配置或索引空缺时无对应块。搜索所有消息，确认正文标记从未自动出现。
- 同时改两个索引，下次请求应只有一条新提醒，只含改动的 scope；原首条前缀逐字节不变。再发一轮，不新增提醒。用普通文件工具写索引再重复；用 CRLF、多字节字符、200/201 行和 25 KiB 临界值检查截断和路径提示。
- 对 summary/native、自动/手动、partial、plan clear context 和 overflow recovery，各在边界前改索引并发送一次，边界后独立改另一个。比较请求与从会话库恢复的重放：当前索引各一次，不重复边界前提醒；仅边界后的改动有新提醒。退出并用同一 data-dir 恢复，比较相同历史前缀的序列化字节。另核对 fork 复制、删除清理及 home/private 项目隔离。
- 运行 core 的 `resident_indexes` 单测、`--test compaction_history --test native_compaction` 及 CLI 的 `--test memory_files --test memory_compaction`；分别记录实际覆盖的边界、协议、请求层和进程层。仅重放测试不能替代真实 provider 请求或 Desktop 验收。

### H. 提示词与工具配置

在数据目录写 `customization.json` 后，用 `dcl -p` 验证：

1. `custom_tools`：模型调用后得到命令的标准输出；`bypass` 下直接运行，其他模式需要授权。
2. 默认请求：没有配置时只带核心工具；`ListDir`、`WebFetch`、`computer_use` 等不在请求里，模型可以用 `ToolLoad` 加载。
3. `tool_loading` 四种组合：启用且按需、只开启用、只开按需、两个都关。两个都关的工具不能加载或调用；只开按需的工具在加载成功后的下一次请求才可调用。
4. 旧 `disabled_tools`：内置工具读作“启用关、按需加载开”。**只有工具页保存**才写入 `tool_loading` 并从 `disabled_tools` 移除内置名称；只保存提示词时旧条目原样保留。非内置条目（例如 MCP 的旧禁用名）始终保留并继续生效：不出现在请求里，直接调用被拒绝。
5. 稀疏保存：只改一个工具再保存，文件里的 `tool_loading` 只新增该条目，未提到的工具继续按默认值或原有旧条目解释。
6. 热变更：模型拿到常驻工具后，在一轮回答进行中把它改为“仅按需加载”，该轮内的后续调用要求先加载；改为两个都关则立即拒绝。已开始的调用不中止。
7. 恢复：重开会话后，只有“模型自己发起、且结果与该调用 ID 对应的成功 `ToolLoad`”恢复加载。用户消息里的同名结构、先结果后调用、ID 不匹配、出错的结果、重复使用或已被消费的调用 ID 都不授权。已改为两个都关的工具不再出现。同一批并行调用中结果顺序不同仍应正常恢复。
8. 真实进程重启：加载工具后退出 `dcl`，用 `--resume` 打开同一会话，首个请求就应包含该工具；在同一数据目录新建会话则不应包含。
9. Responses 协议：使用带 Responses 适配的供应商模型，确认首请求不含按需工具、加载后的下一请求带真实参数结构、工具能实际执行，且恢复后的首个请求已带此前加载的工具。
10. 运行中改配置：在模型响应期间把常驻工具改成“仅按需加载”，该轮后续调用应被拒绝并提示先加载，且下一次请求不再带该工具。
11. `tool_descriptions`：模型读到修改后的说明，`ToolLoad` 返回相同的工具说明；未设置 `tool_summaries` 时，可加载目录里的这一行也使用该说明。
12. `system_prompt`：`replace` 和 `append` 生效，环境信息仍然存在。
13. 文件语法错误时这一轮报错退出；删除文件后恢复默认。
14. 按需加载后的权限：先加载一个需要审批的工具（例如 `Write`），拒绝审批时工具不执行、目标文件不产生，拒绝结果进入下一次请求；审批等待期间取消本轮，不执行工具也不再发起请求。
15. 请求级一致性：加载后让某个请求先遇到一次可重试错误，同一次逻辑请求的各次 HTTP 尝试携带相同的工具列表；请求快照的工具条目应逐项包含实际发送的名称、说明和参数 schema，token 估算应由该请求体重算得到相同结果；只有下一逻辑请求才按最新加载集合生成新列表，且加载后 schema 字节数确实增长。覆盖 OpenAI 与 Anthropic，并确认 Responses 适配结果。
16. 实际压缩：先成功加载一个按需工具，执行一次真实压缩后重开会话，首个请求不再带该工具（摘要不能授权），模型重新加载后下一请求恢复。
17. Responses 真实进程重启：用隔离数据目录 `config.json` 配置 Responses 适配供应商（模型 `api` 为 `openai-responses`，内置供应商 ID 为 `openai`），真实 `dcl` 进程加载后退出，`--resume` 新进程的首个 Responses 请求应包含此前加载的工具。不要用 `--base-url` 覆盖，它会清除内置供应商 ID。
18. 命令改开关：在隔离数据目录执行 `dcl config set tools.<name>.enabled|on_demand true|false`，`dcl config show` 的 `tools` 项与文件内容一致；下一次模型请求的工具集合按新开关变化（两个都关的工具不出现且不能加载，只开启用的工具直接出现在请求里）。同一次写入把 `disabled_tools` 里的内置名折算成开关，非内置名保留。交互终端执行 `/tools`，面板以 `Built-in / Skills / Mcp / Plugin` 四个标签打开：`Built-in` 用 `Space` 改“启用”、`Tab` 改“按需加载”，并按 `Files & workspace`、`Shell & system`、`Web & services`、`Agents & delegation`、`Tasks & plans`、`Automation`、`Context & interaction` 分组；未分类的新工具落入 `Other`，`every_builtin_tool_has_one_group` 会在新增工具未归类时失败；切换后只重绘标题行和被改的那一行，**列表其余内容必须仍然可见**（不能只剩标题和一行）；`Skills` 复用技能行；`Mcp` 复用服务器行（启动失败标记在行上，详情里给首行错误）；`Plugin` 只列插件包。关闭面板后文件与面板一致；非终端下 `/tools` 打印文本清单。`/skills` 打开同款整页面板（`Installed / Errors / Stats`，只含技能）；`/plugins` 打开同款面板但只列插件包；`/skills state`、`dcl config set tools...` 的脚本路径不变。`crates/cli/src/tool_settings.rs` 的单元测试覆盖只改一个开关、双关、迁移保留非内置名和非法输入；`plugins_panel::tests`（`cargo test --locked -p deepseek-code-cli --bin dcl plugins_panel::tests`）覆盖三种标题的标签栏、scope 过滤、插件统计和 MCP 失败标记。

19. 精确名称：`ToolLoad` 接收 `{"names":["WebSearch","ListDir"]}`。数组含 1–64 个非空字符串，名称区分大小写，重复项按首次出现的位置处理。已在请求中或已加载的工具作为 `already_available` 返回，避免重复加载。未知或不可用的名称逐项报告；调用报错且本次不加载任何工具。测试还验证空数组、字符串、额外字段、大小写错误、关键词及 `select:` 输入。

20. 工具目录：每次真实请求的系统提示词（OpenAI 的 system 消息、Anthropic 的 `system`、Responses 的 `instructions`）包含当前可加载的内置工具名和一句话用途，并说明 `ToolLoad` 只能加载这里列出的内置工具。目录与本次请求的工具 schema 来自同一份设置和目录快照，描述覆盖一致。内置目录省略常驻、已加载、两个开关都关、代理未允许及自定义工具。MCP 按需目录使用相同的加载资格判断，省略已加载、旧禁用列表中的工具及代理未获授权的工具。两部分都要求本次请求携带 `ToolLoad`。成功加载后，定义从下一次请求起可调用，本次响应中的调用会被拒绝。是否声称可加载取决于本次请求是否真的有可调用的 `ToolLoad`：两个开关都关或按需但尚未加载时，目录说明它不可用且不列出可加载名称；有 `ToolLoad` 但没有可加载工具时如实说明；受限代理没有 `ToolLoad` 时仍只直接暴露已授权的按需工具。已删除的 Git 工具和 `smart_project_scan` 不在目录中，直接调用返回 `Unknown tool`；Git 命令仍可通过 `Bash` 在临时仓库中执行。目录每行按 `tool_summaries`、`tool_descriptions`、工具自身 `summary`、完整 `description` 的顺序取文本；只折叠空白，不截断也不加省略号。内置工具各自带一条 `summary`（缺少时 `tools::summary_tests::every_advertised_tool_carries_a_listing_summary` 失败），MCP 与自定义工具没有 summary，显示完整说明。自动回归：`cargo test --locked -p deepseek-code-core --lib tool_loading_tests::every_transport_sends_a_dynamic_directory_of_loadable_builtins`、`tool_loading_tests::a_request_without_callable_tool_load_does_not_promise_loading`、`tool_loading_tests::an_empty_directory_is_reported_truthfully`、`tool_loading_tests::removed_git_tools_cannot_be_restored_configured_or_called`、`tool_loading_tests::the_loadable_reminder_shows_custom_text_uncut_and_the_whole_description_otherwise`、`tools::summary_tests::every_advertised_tool_carries_a_listing_summary` 与 `tools::directory_edit_regressions::git_commands_run_through_bash_after_the_native_tools_are_removed`。

21. 显示：SDK `system/init` 和模型工具列表使用 `ToolLoad`。CLI 普通模式、进度行、紧凑分组以及恢复后的滚动记录显示 `ToolLoad(Read, Grep)`。长列表截断且保留右括号。64 列的紧凑模式保留 `ToolLoad(…)`，展开和折叠两种状态均可验证。回退活动行显示带名称的调用标签。浏览器工作明细和轨迹使用相同名称格式。

自动回归：`cargo test --locked -p deepseek-code-core --lib -- tool_loading_tests tool_loading_policy_tests tool_loading_restore_tests code_prompt_tests prompt:: services::customization`，以及 `cargo test --locked -p deepseek-code-cli --test sdk`。
provider mock 直接驱动公开 executor，覆盖 Anthropic、Chat Completions、Responses：
首请求不带按需工具，ToolLoad 成功后下一 HTTP 请求包含真实 schema 并执行 ListDir；
在途时双关和重新启用更新后续请求、拒绝失效调用。损坏配置阻止下一 HTTP 请求。另在成功加载 `ListDir` 后，第二次 HTTP 响应期间分别切换双关、仅常驻和损坏配置，核对当次执行以及下一请求是否发送、是否携带该定义。
同一逻辑请求的 HTTP 重试固定工具定义，快照与实际请求工具集合一致。
恢复回归另从关闭后重新打开的 SQLite 读取成功的 `ToolLoad` 工具行，传入只有显示文本的历史；
验证首个真实请求恢复工具、当前开关及代理限制仍生效，错误/非助手/其他会话/压缩边界前记录不授予加载资格。
这些 core 结果不能代替终端 `/tools` 同会话联动或 Desktop 原生界面验收。

### P. 外部代理（Codex / Claude Code / dcl）

`tools/external_agents.rs` 的会话、线程身份与最终回答契约。自动化使用本机真实 Codex（0.163.0）与本地 Responses mock、隔离的 `CODEX_HOME` 和假密钥：

```bash
cargo test --locked -p deepseek-code-core --lib tools::external_agents::tests
python3 -I codex-real/dcl_codex_harness.py --binary target/debug/dcl --out <新目录>
```

1. 会话 ID 规范化：`session_id` 接受大写、无连字符、`{…}` 和 `urn:uuid:` 等等价写法，启动前统一为小写带连字符 UUID；argv（`--resume` / `resume <id>`）和之后的一致性比较使用同一个规范值。非法 ID 在启动进程前报错。真实 Codex 的 D1/D2/D3 场景都应只有 2 次请求、两次结果报告同一线程 ID，且不出现 “not the requested session”。
2. 线程冲突：JSONL 中第一个 `thread.started` 的 ID 被保留；再出现语义不同的 ID 时失败并报告两个 ID，同一 UUID 的不同写法可以接受。续接时请求的 ID 出现在最后，也不能覆盖先前的冲突。可恢复的 `error` 事件之后仍正常 `turn.completed` 且退出码为 0，不算失败。
3. 最终消息：优先使用 JSONL 的 `agent_message`；没有时读取 `--output-last-message` 文件。两者都没有的空回答仍然成功，并带 `note: Codex completed without a final message`；文件无法读取时明确报错，不伪造回答。非零退出或缺少 `turn.completed` 仍然是失败，即使文件里有内容。

### Q. 终止信号清理（SIGTERM / SIGHUP）

`crates/cli/src/signals.rs` 在运行时启动后、任何一轮开始前注册 Ctrl-C、SIGTERM、SIGHUP。三种信号都走已有的协作取消：当前回合的 engine 任务被 abort 并等待结束，前台 Bash、hook、外部代理的进程树由 `ProcessTree` 的 Drop 结束；之后进程以非零状态退出，不能是“被信号直接杀死”。三者共用同一个通知通道：等待者在开始等待后收到一次通知，之后的回合仍可再次被打断。Windows 只有 Ctrl-C，但也通过同一通道发布，每次 Ctrl-C 都会唤醒当前等待者。

注册失败（例如某个信号无法安装）时启动直接报错并以非零状态退出，不会带着部分安装继续运行。原因是 Tokio 在丢弃某个 `Signal` 时不会恢复该信号的默认处理；部分安装后只监听 Ctrl-C 可能让 SIGTERM 被静默忽略。自动化用注入的失败验证注册在第一个错误处停止并返回错误，不安装真实 OS 处理器。

1. 前台 Bash：模型调用 Bash 运行 `sh -c 'sleep 30 & wait'`，确认 sleep 子进程存在后给 dcl 发 SIGTERM 或 SIGHUP；dcl 必须非零退出且不是被该信号终止，sleep 及其 shell 全部消失。
2. 外部代理：本地假 `codex`（PATH 前置，`customization.json` 中 `Codex` 启用且常驻）启动 `sleep 30 & wait` 进程组；发 SIGTERM 后 dcl 与假 codex、sleep 都不能残留。该用例只检查它自己观察到的 pid，不使用按名称的全局 kill。
3. 真实 Codex 0.163.0：第二模型复验脚本的 B3（SIGTERM）连续 3 次都必须先观察到 `shell_child_seen`，退出码非零，`shell_child_survivors` 与 `codex_survivors` 为空。B1（超时）与 B2（SIGINT）保持通过。补充脚本 `round09-sighup-real-codex.py`（开发支持目录）用同样结构验证 SIGHUP；它不是原复验脚本的一部分。
4. Ctrl-C 语义不变：运行中仍只取消当前回合；REPL 空闲时仍清空未发送的多行草稿并继续，不会退出。SIGTERM/SIGHUP 在空闲或运行中都应结束提示符并以非零状态退出。
5. 只结束本会话拥有的活动工作：故意的 `run_in_background`/`detach` 后台任务和无关进程不受影响，没有全局 kill。
6. Windows 手工验证（本轮未执行，需在真实控制台完成）：`--print` 模式让模型运行一个前台 `sleep`/`Start-Sleep`，按 Ctrl-C 后回合取消、进程非零退出、子进程消失；在交互 REPL 中运行长工具时按 Ctrl-C 只取消该轮，空闲且有未发送多行草稿时按 Ctrl-C 只丢弃草稿并继续。这两次 Ctrl-C 都必须被观察到，不能只在第一次生效。

自动回归：

```bash
cargo test --locked -p deepseek-code-cli --test interrupt_signals
cargo test --locked -p deepseek-code-cli --bin dcl -- signals::
```

`interrupt_signals` 的六个用例分别覆盖 Bash+SIGTERM、Bash+SIGHUP、Codex+SIGTERM、启动中的 Codex 只收到一次 SIGINT，以及两条夹具回归（清理过程中的晚 fork、错误继承 token 的重设）。前三个用例确认取消与进程树清理，第四个用例覆盖“事件处理间隙”：`run_timed_turn` 在函数开始时创建一次 `signals::interrupt_scope()`，信号基线在创建时同步取得；改回每次 `tokio::select!` 新建 `signals::interrupted()` 时，若 Ctrl-C 在 select 结束、事件处理（stream-json 输出、transcript 写入）期间到达，下一次等待会把已经加一的计数当作自己的基线，信号丢失，回合继续。该用例用 `fcntl(F_SETPIPE_SZ, 4096)` 请求 4 KiB 的 dcl stdout 管道容量，再用 `F_GETPIPE_SZ` 读回实际容量并断言其为正、≤ 65536 且小于填充记录的长度（大页宿主机可能把请求向上取整，所以断言针对读回值），接着用 110 KB 的 prompt 让 `tool_start` 记录远超该容量，使 dcl 阻塞在事件处理的同步写入中；在此时只发一次 SIGINT，dcl 必须在 10 秒内以非零状态退出，且假 codex 启动器及其 `sleep 120` 子进程都不存活。夹具进程的环境里带有按运行生成的 token：假 codex 启动器发现自己的 token 不是本轮的准确值就 `exec` 重设（pid 不变），所有后代继承该值，因此调用方环境里的旧 token 不会让整棵进程树变得不可发现。发信号前用例还要求启动器和子进程都出现在按 token 的发现结果里（标记文件本身不能替代该前置条件）。清理按 token 扫描 `/proc/<pid>/environ` 找到进程，先打开 pidfd 再复核 token，只用该描述符发送 SIGKILL，因此扫描之后被复用的 pid 不会被误杀；清理服从单一 2 秒截止（晚 fork 用例用自己的有界预算），只有成功扫描连续为空满 150 ms 才算成功，否则在结果中报告 “cleanup deadline expired before an empty interval was confirmed”，不会因为最后一次扫描恰好为空就当作干净。僵尸（`Z`/`X`）不计入残留，产品断言前有 2 秒回收等待，且该断言先于任何守卫清理执行，否则会掩盖产品回归。dcl 由 `child.wait()` 回收后不再另行 kill。两条夹具回归分别验证：`cleanup_rescans_for_a_child_forked_after_discovery` 在第一次快照之后才释放被门控的启动器，就地用 pidfd 固定已被 token 认证的晚 fork 子进程，之后要求后续扫描发现并停掉它，并以“pidfd 报告进程已消失或 `Z`/`X` 状态”判定它不再运行（孤儿僵尸不算存活）；`the_launcher_replaces_a_wrong_inherited_token` 用每次运行唯一的错误 token 启动，并为该 stale token 单独创建一个守卫，即使启动器的重设回归也不会让 `sleep 120` 逃逸。两条独立夹具在启动任何进程之前都先预检 pidfd 能力。**该用例与上述夹具仅限 Linux 的 x86_64/aarch64 64 位目标**（pidfd 系统调用号 434/424、`/proc` 与 Linux 的单个参数长度上限；指针宽度排除 x32/ILP32 变体），启动夹具前先用自身进程预检 pidfd 能力，缺失时以明确信息失败。把它换回旧的每次新建等待时该用例会失败（旧代码挂起直到超时），用于确认测试覆盖该缺陷。`signals::tests` 另外覆盖平台无关的通知语义：多个等待者同时唤醒、旧信号不唤醒新等待者（新 scope 不继承旧信号）、终止信号在等待者出现前到达仍被记住、Ctrl-C 不触发进程退出、scope 在首次 poll 之前或两次等待之间收到的信号不会丢失、同一 scope 连续两次等待都报告已观察到的中断、计数器回绕（从 `u64::MAX` 起）仍能检测、`rearm()` 之后只等待下一个信号，以及注入的部分注册失败会在第一个错误处停止。worker 观察循环另有两条真实信号用例（Linux）：`worker_frontend::failure_tests::a_sigint_during_a_tick_cancels_the_worker` 在子进程里运行真实的 `observe`，先把 stderr 管道用 `F_SETPIPE_SZ` 请求 4 KiB、断言读回值低于 200000 字节，并 `OBSERVER-NOTICE-` 处停止读取，使观察者阻塞在 tick 的同步输出里（此时不轮询 select），再发一次 SIGINT，放开管道后要求任务目录出现 `cancel`——若把 scope 建在循环内部，这次信号会丢失，用例失败；`a_termination_does_not_make_the_observer_spin` 在 SIGTERM 后确认 `cancel` 出现，并比较 1 秒前后 `/proc/<pid>/stat` 的 utime+stime，确认粘性终止信号不会让循环空转。

#### Q1. Linux PTY 断开

```bash
cargo test --locked -p deepseek-code-cli --bin dcl --test standalone -- close_during_streaming_output redirected_output --nocapture
cargo test --locked -p deepseek-code-cli --test local_coordination_registry terminal_hangup:: -- --nocapture
cargo test --locked -p deepseek-code-cli --bin dcl -- --ignored --exact terminal_output::tests::redirected_output_file_eio_screen_reader_turn
cargo test --locked -p deepseek-code-cli --bin dcl -- --ignored --exact terminal_output::tests::screen_reader_partial_write_and_full_session_volume
```

用隔离数据目录和本地 mock 在 Linux PTY 启动交互式 CLI。分别在空闲、回合中、启动确认界面及等待光标位置时关闭 master，并单独发送 SIGHUP 和空闲 SIGTERM。关闭非控制 PTY 时不会收到内核 SIGHUP，也必须取消。每次最多等 5 秒，使用 `wait4` 核对关闭阶段的 CPU 时间小于 700 毫秒，退出码为 1；空闲和回合中检查 frontend 注册记录已关闭，回合中检查用户消息及取消状态已写入数据库。 两个流式回归分别在读屏 stdout 和普通渲染 stderr 写入阻塞时断开 PTY，独立检查所有线程均未 panic、完整 assistant 增量已保存、SessionEnd 收到同一会话的 other 原因，以及退出耗时和 CPU 上限。

重定向对照覆盖控制 PTY、另一个已挂断的 PTY、普通文件和 `/dev/full` 配置确认。读屏回合的 ENOSPC 与定向注入文件 EIO，以及输出路径与干净基线的比较脚本，见开发记录 `dev-support/deepseek-code/pty-hangup-20261119/report.md` 的 Round 4。

共享卷用例需要可在普通用户下创建 tmpfs 的 `bwrap` 和 `strace`（可用 `STRACE` 指定路径）。用例创建独立的 16 MiB tmpfs，在模型回复前留下 32 KiB，核对 stdout 和 SQLite WAL 都出现 ENOSPC、回复只写入一部分、进程以 1 退出且未再提示输入。仅 WAL 写满与普通 flush 失败的基线对照见同一记录的 Round 5。

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

### N. 模型列表刷新

`/model refresh` 刷新当前提供商的模型列表，`/provider refresh [ID]` 刷新指定提供商（省略 ID 时刷新当前提供商）。刷新以上游返回的模型 ID 为准：新增上游新出现的模型，移除上游不再返回的模型（包括手工添加的）。同一 ID 保留本地名称、启用状态、上下文窗口和协议等设置；新模型使用现有模板并默认启用。

自动检查：

```bash
cargo test --locked -p deepseek-code-cli --bin dcl interactive_config::tests
cargo test --locked -p deepseek-code-cli --bin dcl refresh_subcommands
cargo test --locked -p deepseek-code-core --lib model_listing
```

覆盖命令解析、增删同步、同 ID 设置保留、重复 ID、当前模型被移除后切换并保留历史、刷新其他提供商不切换会话、默认模型失效（含刷新前就已失效）与无效默认不写、取消不保存、会话切换失败和默认偏好写入失败后的回滚，以及刷新期间配置被并发修改：`refresh_aborts_when_the_provider_is_removed_meanwhile`（目标提供商被删除时中止，删除结果保留且不写入）、`refresh_aborts_when_the_provider_changes_meanwhile`（目标提供商被修改时中止，并发编辑保留且不写入刷新结果）、`refresh_keeps_a_concurrent_change_to_another_provider`（其他提供商的并发修改与本次刷新结果都保留），以及 `/model r`、`/provider r` 的子命令补全与 provider ID 补全。

`model_listing` 的本地 HTTP fixture 分协议覆盖：OpenAI、Anthropic、Gemini 的认证；Anthropic 与 Gemini 的多页遍历和游标循环；OpenAI 兼容接口单页返回，以及它声明分页时安全拒绝；非法条目、错误类型的 `has_more`/`nextPageToken`、空列表、失败分页、非 JSON、200 错误对象、重复或缺失游标、超时。错误与游标都不回显凭据或令牌：Key 的原样、URL 编码（大小写十六进制、逐字符混合编码）、Base64/URL-safe Base64 形式都会被替换为 `[redacted]`，Base URL 路径中出现的 Key 同样会被隐去。

`cli.json` 通过临时文件加改名写入，权限为仅所有者可读写。`config::search_tests` 的 `saving_preferences_keeps_the_previous_file_when_the_commit_fails` 校验提交失败时旧文件字节不变、没有残留临时文件；`a_failed_default_save_restores_the_list_and_selection` 在完整刷新流程中注入同一失败，校验模型列表、会话和偏好文件都恢复。

真实终端验收（使用隔离数据目录和本地 mock 端点）：

1. 保存一个提供商，启动 dcl 后执行 `/model refresh`：输出新增、移除和保留的数量以及各自的模型 ID，使用普通文本列表。随后直接执行 `/model`，无需重启即可看到新增模型。
2. 让当前模型从上游列表消失后再次刷新：保存前出现替代模型选择；用方向键选择并按 `Enter` 确认后，当前会话切换过去，对话历史保留，`/model` 显示新列表。
3. 在同一场景按 `Esc`，或在文本选择模式下直接回车（没有选择列表时）：整次刷新取消，`config.json` 字节不变，当前模型和默认模型都不变。有选择列表时 `Enter` 是确认，不是取消。
4. 用 `/provider refresh <另一个 ID>` 刷新非当前提供商：模型列表更新，当前会话的提供商和模型不变。
5. 重启 dcl：刷新后的模型列表仍然存在；被移除的模型不再出现在 `/model`。
6. 对没有 Base URL 的多协议提供商、未保存的临时提供商、未知 ID 和多余参数分别执行刷新：都应给出明确错误，配置不变。
7. 断开 mock 端点、返回空列表、只返回第一页后让下一页失败、上游返回缺模型 ID 或错误类型的分页字段、整次查询超过 20 秒总预算：都应报告失败，`config.json` 不变，错误信息不包含 API Key。查询进行中按 `Ctrl+C` 应立即显示 `Refresh cancelled; nothing was changed`，同样不写入。
8. 补全：输入 `/model r` 按 `Tab` 得到 `/model refresh `；输入 `/provider r` 按 `Tab` 得到 `/provider refresh `；输入 `/provider refresh ` 按 `Tab` 补出已保存的 provider ID。

OpenAI/Responses 的模型接口通常一次返回全部模型。它们声明还有下一页时，dcl 会拒绝这次刷新并保持配置不变，不会只同步第一页；Anthropic 和 Gemini 会读完所有页。

Windows ConPTY 使用独立数据目录重跑第 1、2、3、5、8 条。当前分支的 Windows 终端验收尚未执行，汇报时必须单独列出，不能用 Linux 结果代替。

### J. 欢迎区与计划模式提示

1. 新会话分别发送普通问题、`/plan <任务>`，以及先 `/plan` 再提问：欢迎区三行保持启动位置，计划模式提示使用 `DeepSeek Code`。
2. 本地模拟接口分别保持流式输出、返回短回答、断开连接：运行中、完成和连接失败后欢迎区不下移，输入框保持底部；第二轮短对话同样检查。
3. 运行中和等待输入时缩放窗口，再切换 `Alt+M` Compact 并返回普通视图：仍可容纳欢迎区时三行保持原位。长输出将欢迎区正常滚出后，最新问题固定栏出现，放大窗口不复活旧欢迎区。
4. 恢复仅含文本回答的会话：最新问题固定栏正常显示，`Ctrl+Home` 能到最早的问题；再检查排队撤回、工具展开和 `Esc` 中断。
5. 在独立数据目录和工作目录中运行 `welcome-plan-acceptance-20261106.mjs`，传入本次构建产物；脚本与真实 ConPTY 文本、原始终端输出及断言结果位于相邻开发支持目录。不得用正在运行的已安装程序替代本次构建产物。

### DeepSeek 平台请求头

- `cargo test --locked -p deepseek-code-core --lib native_engine::request_options::tests`：平台只按 builtin_id=deepseek 或 URL hostname=deepseek.com / *.deepseek.com 判断，与模型名、api_format 无关；排除第三方 DeepSeek 模型、相似主机和 deepseek.com.evil.example。三通道真实本地 HTTP 检查有/无会话、普通/压缩矩阵和既有认证/身份头；直连客户端仅在测试 HTTP client 用 DNS resolve 指向本地 mock。
- `cargo test --locked -p deepseek-code-cli --test deepseek_harness_headers`：两个 api_format 的真实 CLI 子进程，用 builtin_id=deepseek + 本地 base_url；普通轮次只带 x-deepseek-harness-session-id，逐字等于 SQLite 会话 ID；超限触发实际压缩后摘要同时带 x-deepseek-harness-compact: 1，恢复的普通请求不带 compact，摘要及边界已持久化。
- 分段、合并、截断后的恢复及重试都走 Summarizer::attempt 的同一显式请求选项；回归 `compaction_truncation`、`compaction_overflow`。标题、权限分类、Hook、WebFetch 回答及会话辅助生成带其所属会话 ID，连接测试/模型列表不带。真实 DeepSeek 服务端及跨平台终端接受情况需单独验收，不能从 mock 推断。

### K. 模型请求失败的诊断与重试

全部使用本机可控的 HTTP/TCP 故障服务，不使用真实密钥或计费接口。自动化分三层：

- `cargo test --locked -p deepseek-code-core --test request_failure`：故障注入、分类、预算、取消与脱敏。
- `cargo test --locked -p deepseek-code-core --test api_failure_audit_probe`：审计发现的三个泄漏输入（多项 Cookie、Basic 代理凭据、服务端回显请求正文）。
- `cargo test --locked -p deepseek-code-core --test api_failure_reaudit_probe`：二次审计的输入（带空白的 JSON 键、短纯文本回显、多种嵌套与转义形式），以及“可识别类别仍保留”的反向用例。
- `cargo test --locked -p deepseek-code-core --test api_failure_review3_probe`：三次审计的输入（`message`、`detail`、`error_description`、`code` 四个字段中的纯自由文本）。
- `cargo test --locked -p deepseek-code-cli --test api_failure_diagnostics`：真实 CLI 进程对本地故障服务，核对最终输出、服务端实际请求数与 SQLite 中的请求快照。
- `cargo test --locked -p deepseek-code-core --lib builtin_chat_`：内置 Chat Completions 的发送重试与“内容前正文重试”共用一个预算，核对真实回环 HTTP 的次数、请求 ID、重试头、可取消等待、已开始内容不重放、原始帧/终端标记/未知内容边界、提供方错误脱敏、损坏配置在开请求前失败与本地请求记录。
- `cargo test --locked -p deepseek-code-cli --test api_failure_diagnostics builtin_chat_`：真实 CLI 使用 `config.json` 中的内置 OpenCode Go，内容前断流后重连成功，覆盖只有思考或未完成工具调用的失败，并核对提供方错误回显不会进入最终输出、尝试事件/日志与 SQLite 快照。
- `cargo test --locked -p deepseek-code-core --lib native_engine::model_http::tests::`：流式客户端的空闲读取超时按每个线上分块重置（注释、跨分块的 SSE 帧与跨分块的 UTF-8 字符），并区分“等待响应头”和“正文静默”两种超时；取消挂起的读取会关闭正文，`request_timeout_secs = 0` 与 `pool_idle_timeout_secs = 0` 都在开请求前报错且 0 次连接；进程级共享连接池在顺序请求及并发后复用同一回环 TCP 连接，`Redirect::Default` / `Redirect::None` / `single_post` 各自保持独立策略。
- `cargo test --locked -p deepseek-code-core --lib config::tests`：`[http].pool_idle_timeout_secs` 默认 300、显式值解析及缺省旧文件兼容。
- `cargo test --locked -p deepseek-code-core --lib buffered_model_requests_keep_the_total_deadline`：直连 OpenAI 与直连 Anthropic 的缓冲请求仍受各自总超时约束，缓慢正文超时后只发送一次且不含 `stream:true`。
- `cargo test --locked -p deepseek-code-cli --test api_failure_diagnostics stream_timeout_uses_idle_configuration_for_all_model_transports`：真实 CLI 在本地正值空闲配置下覆盖四种模型传输的健康流、内容后静默、只有响应头、两次 POST 恢复、内置 Responses 单次与退避取消，以及内置 Chat 的原始边界。
- `cargo test --locked -p deepseek-code-core --lib typed_stream_body_timeout_survives_the_retry_layer`：正文读取超时、终端标记、提前 EOF、提供方超时形状的 SSE 错误与响应头前超时在共享重试层后的类型化事实（见下方第 18 条）。
- `cargo test --locked -p deepseek-code-core --lib single_attempt_chat_stream_never_spends_a_second_post`：内置与直连 Chat 传输在单次预算下，503、响应头停顿、空握手正文超时与 307 重定向都只产生一次物理 POST（见下方第 18 条）。
- `cargo test --locked -p deepseek-code-core --lib post_content_timeout_recovery_defaults_to_false`：可选项的内置默认、缺省该键的旧文件、显式 `false` 与显式 `true` 的解析结果。
- `cargo test --locked -p deepseek-code-core --lib openai_counted_stream_invalid_orchestration_fails_before_open`：内置 OpenCode Go 与直连 Chat 都在各自的全新进程中，经 `send_message_stream_counted` 使用损坏的 cwd `orchestration.toml`：返回真实 TOML 错误且本地服务收到 0 次 POST（`builtin_chat_invalid_orchestration_fails_before_open` 保持为对照）。
- `cargo test --locked -p deepseek-code-cli --test api_failure_diagnostics post_content_`：第 19 条的开启/关闭、可重放边界与取消（见下方“第十三轮”）。
- `cargo test --locked -p deepseek-code-core --test post_content_recovery post_content_recovery_receiver_close_cancels_backoff_open_and_read`：真实执行器在退避、替换开打与替换读取三个阶段遇到消费端关闭时立即返回 `interrupted`（见下方第 19 条）。
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
9. 流式响应在收到部分内容后正常结束但缺少完成标记：阶段为流式中断、HTTP 状态为已收到的 200、尝试次数为发送层真实值。OpenAI、Anthropic 与内置 Chat Completions 都要报告真实次数与状态；Responses、Anthropic（内置）等其它内置协议仍按发送层单独重试，正文读取不再计入预算，次数与状态保持 `unknown`，不能用推测值替代。
10. 内置 OpenCode Go 的 Chat Completions 先返回 503，再返回 200 且只有响应头、正文停住，最后成功：共 3 次 POST，同一请求 ID 与重试计数 0/1/2，保留 OpenCode 会话头，两次重连都等待，正文只出现一次；若每次都在内容前结束且只有空握手，则 8 次 POST、7 次等待后报告 HTTP 200、尝试 8、重试 7、`content_started=false`，退避期间取消只发送 1 次。只有思考、未完成工具调用、被截断的原始 SSE 帧、未知数据或显式提供方错误都只发送 1 次。
11. 内置 Chat Completions 的原始数据边界以“原始帧”而不是归一化结果判定：未完成的原始 SSE 帧、单独的 `[DONE]`、以及只含未知类型的非空 `content` 数组，在正文被中断（chunked 未结束）或一直保持到注入的客户端超时后，都只发送 1 次 POST；第 2 个 POST 即使有成功回复也不会发出，不出现重连等待、替换正文或完成标记，失败为流阶段、HTTP 200、尝试 1、重试 0；超时用例的类别为 transient 且原因包含超时。干净 EOF 的半帧仍只发送 1 次，正常完整流仍只发送 1 次并成功。`max_retries=1` 时先 503、再 200 只有角色握手的干净 EOF、第 3 个请求有待发的成功回复：共 2 次 POST、1 次等待，最终为流阶段失败、HTTP 200、尝试 2、重试 1。
12. 内置 Chat Completions 的提供方 SSE 错误即使在 `error.message` 里回显请求内容，也只发送 1 次并退出 1；最终输出、`request_attempt/stream_failed` 事件、磁盘上的尝试日志与 SQLite 请求快照都只保留固定词表（如 `invalid_request_error`）与安全原因，回显标记不出现，失败为流阶段、HTTP 200、尝试 1。
13. 独立内置 Chat 传输在未初始化进程中使用损坏的 `config/orchestration.toml`：返回配置错误且在打开任何请求之前结束，本地服务收到 0 次 POST；同时核对正常配置下共享预算仍按 `max_retries` 生效。
14. 流式模型请求按“每次读取的空闲超时”计时，没有整段总超时。`[http].request_timeout_secs` 设为 1 秒时，内置 OpenCode Chat、直连 OpenAI、直连 Anthropic 与内置 Responses 都能在约 3 秒内持续收到字节后正常完成：各 1 次 POST、退出 0、输出完整；该值必须为正，0 在建立请求前就报配置错误且不产生 POST。缓冲式请求（非流式 `send_message` 与 Google 令牌交换）仍使用 300 秒总超时：缓慢输出但总时长超过期限的 JSON 正文仍以超时失败，只发送 1 次且请求体不含 `stream:true`。
15. 内容后停止发送：1 次 POST、退出 1、已输出的部分正文保留一次、失败为流阶段；即使还有重试预算也不重放。已刷新 HTTP 200 响应头但正文 0 字节：内置 Chat 与直连 OpenAI/Anthropic 在预算内重连（两次 POST 的第二次成功），内置 Responses 仍按发送层策略只发送 1 次。共享层可观测时报告 `http=200`、尝试 1，其余内置协议保持 `unknown`；退避取消以尝试日志中的 `retry_wait` 为起点写后台任务取消文件，之后不再产生新的 POST。
16. 内置 Chat 的三类原始边界（未完成的原始 SSE 帧、单独的 `[DONE]`、只含未知类型的非空 `content` 数组）在空闲超时生效后仍各只发送 1 次，尝试日志中没有重连等待，细节为流阶段、HTTP 200、尝试 1、重试 0。
17. 单次 `--print --output-format stream-json`（纯文本输入）在收到部分正文后停止发送：1 次 POST、退出 1，结果带 `stream_error`，部分正文只保留一次，持久化恰好一个 failed 回合和一条 `request_turn_end`。随后用 `--resume <同一会话>` 对成功回复再运行一次：原 failed 行保持不变，新回合 completed，两次调用共用同一存储会话，第二次退出 0 且只新增 1 次 POST。
18. 内容后恢复的传输准备（第 13 轮；功能默认关闭，配置、工具循环重放与可见提示留到第 14 轮）。结构化失败携带两个独立的内部事实：`is_stream_body_timeout()` 只有“流阶段 + reqwest 正文读取超时”为真，响应头前超时、提前 EOF、提供方 SSE 错误与解析错误都为假；`terminal_marker_observed()` 记录解码器在正文停止前是否已看到原始 `[DONE]`，与超时事实互不推导。内置 Chat 在真实文本后收到原始 `[DONE]` 再停住时两个事实都为真，只有文本超时时终端事实为假；失败 JSON 字段与脱敏结果不变。单次预算（`AttemptBudget::single_attempt()`）下，内置与直连 Chat 对 503、响应头停顿、已刷新空握手正文超时和 307 都只发 1 次 POST、没有 `retry_wait`，307 的目标收到 0 次；健康流只发 1 次、输出完整，认证头、请求身份头、重试计数与本地请求记录仍走原路径。证据见 `dev-support/deepseek-code/dcl-stream-retry-20261117/round-04-report.md`；可选重放本身见第 19 条。
19. 内容后超时的一次可选重放（第 14 轮，`[http].post_content_timeout_recovery`，默认 `false`）。开启时只对内置与直连 Chat Completions 生效，整个回合最多一次，且只重放“尚未提交的当前模型步骤”：
    - 关闭（省略该键或显式写 `false`）：文本/思考仍实时输出；正文在内容后超时只发送 1 次 POST、没有重放提示，最终结果 `is_error=true`、`stop_reason=stream_error`、部分正文只保留一次，未完成的工具调用不执行、不落行，回合为 failed、退出 1。
    - 开启：失败的那一步丢弃本步暂存的文本、思考与未完成工具参数，快照带 `abandonedForRecovery=true` 并保留诊断用的部分块；随后通过既有 `request_attempt` 通道发出且仅发出 1 条 `outcome=post_content_retry` 提示（含被放弃的 request ID、真实尝试次数与脱敏失败），等待既有的、可取消的退避后只发 1 次替换 POST。替换请求沿用同一会话、同一回合、同一模型步骤号，`replacesRequestId` 链接两张快照；`MessageStart` 仍只有一次，被放弃的那条不产生终止结果。替换成功后该步骤正常提交文本并继续工具调用与持久化；成功回合的最终状态为 completed，且不挂被放弃尝试的失败。
    - 只有 `class=transient`、`is_stream_body_timeout()` 为真、当前响应已观察到内容、`terminal_marker_observed() != Some(true)`、请求快照未记录 `[DONE]`/任意 choice 的 `finish_reason`/提供方 SSE 错误、消费端仍在，且首段真实尝试次数小于 `AttemptBudget::configured().max()` 时才重放。替换本身只有一个 POST（响应头停顿、重定向都不额外发）。替换再次失败、后续步骤超时或预算已用尽都不再重放，只提交最后一次尝试的暂存输出并走原有失败路径。
    - 开启期间该步骤的文本/思考在步骤完成时才出现（实时增量暂时缺失）；取消发生在提示之后、替换之前时不发替换 POST，发生在替换正文打开期间时不再读取、也不再发 POST，未提交的暂存输出被丢弃。
    - 第 14 轮修复（第 15 轮验证）：直连 Chat 的两种提供方错误形态（嵌套 `error` 与顶层 `{"type":"error"}`）都在正文随后真的空闲超时时否决重放；文本/思考/工具增量出现在非首个 choice 时同样算作已产生内容并允许重放（仅角色或空增量仍走 A 的内容前重试，且请求 ID 不变）。关闭（省略或显式 `false`）时快照键集与 `stop_reason` 与第 13 轮对照完全一致：`message_trajectory.requests_json` 的 41 个键（第二个模型步骤多 `responseModel`），既没有 `replacesRequestId` 也没有 `abandonedForRecovery`，结果 `stop_reason` 为第一步的 `stop`、正文为 `Before \n\nAfter`。消费端关闭会在退避、替换开打与替换读取三个阶段立即返回 `interrupted`，不再等待计时器或空闲超时。
20. 进程级流式连接复用（`[http].pool_idle_timeout_secs`，默认 `300` 秒）：
    - 所有生产流式模型请求共享 `model_http` 中的三个进程级连接池（`Redirect::Default`、`Redirect::None`、`single_post`），每池每 host 最多保留 32 条空闲连接；不再按请求新建流式客户端。
    - 同一回环 keep-alive 服务器上连续两次流式 POST 只触发 1 次 TCP `accept`；4 个并发请求结束后再发顺序请求复用已有空闲连接、不新增 `accept`。
    - `Redirect::Default` 与 `Redirect::None` 保持隔离，`single_post` 既不跟随重定向也不触发内部重试；`pool_idle_timeout_secs = 0` 在发起任何请求前报配置错误且产生 0 次连接。
    - 手工/本机 mock 验证：默认配置下同一服务地址跨轮次连续发送两条消息只建立 1 条连接；`pool_idle_timeout_secs = 1` 等待超过 1 秒后触发新 `accept`；取消正在输出的流后，下一条消息不复用被取消的连接。

同一目录的 `desktop/fixture.py` 配合真实 Desktop 窗口核对错误卡片上的阶段、次数与脱敏结果。

### L. 超长历史的摘要分段、每段恢复与分层合并

全部使用本地模拟接口和临时数据库，不发真实请求。

- `cargo test --locked -p deepseek-code-core --test compaction_cache_prefix`：主回合完成后重开引擎，通过本地 Chat、Anthropic Messages 和非 GPT Responses 接口比较完整请求体。摘要须保留主回合的系统指令、工具定义、首条消息的提醒和后续回答，末尾只追加一条含 keep 的用户指令。即使回复同时含正文与写文件调用，也只能提交正文；历史只增加摘要边界。按请求 ID 核对 SQLite 的输入和缓存读取 token 数，包括截断后的各次恢复请求。
- `cargo test --locked -p deepseek-code-core --test compaction_history`：检查本地压缩和首轮收件箱投递后的历史完整性、Responses 不透明窗口的分段回退、结构化输出约束、Claude 推理模式下的前缀复用，以及原生网关的路由和鉴权。核对每日缓存分项、跨分支的物理请求数和 Bedrock 缓存读写。
- `cargo test --locked -p deepseek-code-core --test native_compaction`：同一 GPT Responses 会话的主请求与原生压缩请求使用相同的 `prompt_cache_key`；分支使用自身会话的键。核对压缩请求可用字段和会话请求指标。真实服务端的命中率需用已启用记录的代理另行测量。
- `cargo test --locked -p deepseek-code-cli --test compaction_summary_stream`：通过本地 OpenCode Go Chat 接口续接超阈值会话。角色帧和正文帧被合并为一个流元素时，摘要按 SSE 帧顺序解码，正文中的 JSON 字面文本原样写入边界，续接主请求只发送一次。若单个提供方帧内含两个 JSON 值，首个分段请求失败，保留原历史和解析位置，不重发、不写入边界、不发送主请求。

1. `cargo test --locked -p deepseek-code-core --test compaction_status_regressions`（3 项）：指令开销的 `unknown_parts` 不为 0 时估算不得标为完整；与压缩边界同一毫秒、但没有记录该边界的旧请求不得成为当前用量；旧分支复制了祖先边界且没有计数键时，次数为“未知（旧历史）”、可证明的自身次数为 0，不显示精确的祖先累计值。
2. `cargo test --locked -p deepseek-code-core --test compaction_boundary_provenance`（6 项，均覆盖 OpenAI 与 Anthropic）：先按当前历史读取消息与压缩边界，再由另一入口提交新的压缩，随后用旧消息发出真实 HTTP 请求。断言服务端正文是旧上下文、不含新摘要，且每个请求快照记录的 `contextBoundary` 都不是新边界；没有可信来源的主机消息记录为未知边界。把执行器改回“发送时查询数据库最新边界”后这 4 项全部失败，证明它们确实覆盖该缺陷。
3. `cargo test --locked -p deepseek-code-core --test compaction_truncation`（41 项）：两个协议下首次摘要请求直接使用所选模型配置的输出上限（缺失或 0 时用 32768 固定回退）且篇幅目标不变、模型上限下按 token 二分并合并、合并再次截断后按同一预算拆分、合并持续截断停在深度上限并复用已完成分段、单段额度耗尽时其它分段继续完成、恢复合并超过 64K 时拒绝发送、恢复结果在失败和重启后复用、合并超出安全输入预算、最后实际预算与拟请求预算分开报告、窗口过小与分段规划失败的完整诊断、非截断立即停止、截断文本不进入缓存、缓存写入失败不提交、并发修改历史拒绝提交、局部摘要截断、约 85 万 tokens 历史的分段与分层合并、约 80 万 tokens 的单段失败隔离与缓存复用、长摘要形成小于 8 份的分组并单项传递、空正文按同一预算重试一次后安装、空正文重试仍为空时进入拆分、超长正文直接拆分且只按同一预算拆分不原样重发、拆分恢复返回空正文时先在同一预算原样重试、重试成功时不再拆分、重试仍为空时按同一预算拆分、拆分恢复返回超长正文时按同一预算拆分且不再重试、持续空正文停在深度上限且不写缓存、持续空正文与持续超长正文的最终错误保留结束原因、字符数、预算与用量、空正文导致的分段失败不影响其它分段并在重试时复用它们、大量分段共用额度时拆分请求未发出仍报告触发它的超长正文、空正文重试后再拆分同样保留诊断、取消后新增真实记录时内容未变的分段不再重发（同进程）、同一场景在关闭并重新打开数据库与引擎后仍只重发改变的分段。多数用例在 OpenAI 与 Anthropic 各跑一遍；`persistent_empty_summaries_stop_at_the_depth_limit_and_cache_nothing` 只跑 OpenAI。
4. `cargo test --locked -p deepseek-code-core --test compaction_overflow`（7 项，1 项按设计忽略）：分段、合并与最终摘要均从首个请求使用所选模型配置的输出上限；显式 8192 配置的每个请求都是 8192；配置为 null 或 0 时首个真实 HTTP 请求使用 32768 固定回退并仍完成提交；2000-token 窗口放不下该回退时发送 0 个请求，历史与缓存不变；原有取消、失败与最终摘要预算场景保持。
5. OpenAI 的 `length` 与 Anthropic 的 `max_tokens` 都按结构化结束原因识别，不依赖错误文字；其他结束原因仍按普通失败处理，不进入拆分，且不新增请求。正常结束但正文为空或超过 32,000 个字符同样是结构化结果，不当作输出上限，也不会保存或替换上下文。
6. 有主回合前缀时，只要模型安全窗口能容纳完整请求并预留输出预算，就先用完整历史生成摘要。没有可复用的主请求、完整请求装不下或需要分段恢复时，沿用以下分段规则。分段与合并请求的完整输入（系统指令、阶段提示词和历史）不超过 64,000 tokens，即使模型窗口能装下整段历史也先分段。该上限覆盖恢复合并：两份子摘要拼起来超过 64K 时，即使 1M 窗口仍装得下，也拒绝发送并只让该分段失败。约 85 万 tokens 的合成历史产生 14 个左右的分段，每个请求都在上限内，随后经 3 次分层合并得到最终摘要。
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

模型可见的技能列表、`Skill` 工具和斜杠列表共用 `skills::discovery::discover(data_dir, workspace)`。阅读顺序（后者覆盖前者）是：内置工作流（`bundled.rs`）、`<数据目录>/skills/bundled`、`<数据目录>/skills`、`<工作区>/.claude`、`<工作区>/.agents`。同一名称只列一次；项目覆盖数据目录，项目内 `.agents` 覆盖 `.claude`。`CLAUDE_CONFIG_DIR`、`~/.claude` 和 `~/.claude/skills` 不再读取，个人技能应放在数据目录。`<数据目录>/skills/bundled` 和 `mcp` 仍会列出（斜杠列表原本就显示它们），但不会覆盖已发现的内置或项目同名技能；`<数据目录>/skills/<名称>` 的直接用户技能仍按“项目覆盖用户、用户覆盖内置”处理。发现过程只读，不创建 `<数据目录>/skills`，不跟随符号链接。

数据目录技能的名称沿用 round 01 之前的斜杠命令规则：取 frontmatter `name`（缺省时为技能所在目录名）转小写、空白折叠为 `-`，其他标点保留。因此 `My Skill` 仍是 `/my-skill`，`R&D (team)'s check` 仍是 `/r&d-(team)'s-check`；多层集合也只使用技能自身名称，不加集合前缀。两个数据目录技能归一化后同名时，两者都改用按 `SKILL.md` 路径计算的稳定 `skill_<hash>`（只有哈希碰撞时才按排序后的路径追加 `-2`、`-3`），不会按文件系统顺序只保留其中一个。`~/.claude`、项目与插件技能的名称规则不变。

1. 自动化：`cargo test --locked -p deepseek-code-core --lib discovery::tests`、`cargo test --locked -p deepseek-code-core --lib data_dir_skill`、`cargo test --locked -p deepseek-code-core --lib runtime_discovery_keeps_pre_round_01_slash_names_for_mixed_case_and_spaces`、`cargo test --locked -p deepseek-code-core --lib runtime_discovery_keeps_punctuation_that_skill_creation_allows`、`cargo test --locked -p deepseek-code-core --lib runtime_discovery_reads_agents_skills_and_prefers_them_over_claude`、`cargo test --locked -p deepseek-code-cli --bin dcl slash_list`、`cargo test --locked -p deepseek-code-cli --test skills`。真实进程用例确认首个请求的 `Available skills` 含数据目录技能（名称已小写化），第二请求的工具结果给出真实技能目录。`discovery::tests` 中的 `installed_placeholder_and_legacy_builtins_are_visible_to_both_views` 还确认 `ensure_installed` 不再写出 `skill-creator` 占位，内置 `skill-creator` 只在模型列表和斜杠列表中各出现一次，五个 legacy builtins 仍按原样可见。
2. 真实终端：用独立数据目录，把 `skills/<名称>/SKILL.md` 只放在数据目录，运行 `dcl --data-dir <目录> --print "/<名称> 参数"`。确认斜杠命令展开、`dcl skills list` 显示该技能；再让模型自行调用 `Skill`，确认工具名与斜杠名称一致，返回的说明包含实际数据目录路径，`references/...` 等相对路径可解析。
3. 同名覆盖：在数据目录、工作区 `.claude/skills` 和 `.agents/skills` 放同名技能，确认列表只有一项并按 `.agents` > `.claude` > 数据目录选择；移除高层副本后依次落到下一层。
4. 开关：`enabled: false` 不出现在列表和工具中；`disable-model-invocation: true` 不在模型列表、`Skill` 工具也不能调用，但斜杠命令仍可用；`user-invocable: false` 仍只供模型调用。
5. 容错：在同一目录放一个格式错误的 `SKILL.md` 和一个有效技能，确认有效技能仍可用，无效文件只产生路径诊断（运行日志），不会回显文件内容；`<数据目录>/skills` 不存在时发现过程不创建它。
6. `--data-dir` 与 `DEEPSEEK_CODE_DATA_DIR`：两者选中的非默认数据目录都生效，模型列表和工具使用同一位置，不回退到 `~/.deepseek-code` 或 `~/.claude`。
7. 内置工作流不得被意外覆盖：在 `<数据目录>/skills/bundled` 放与内置同名的技能，确认仍执行内置定义；直接放在 `<数据目录>/skills/<同名>` 的用户技能按既有规则覆盖内置。插件命名空间技能的行为不变。
8. 名称兼容：把 frontmatter `name` 写成 `My Skill` 或含 `(`、`&`、`'` 的名称，确认斜杠命令分别为 `/my-skill`、保留标点的旧名称，模型列表和 `Skill` 工具使用同一个名称，不存在大小写不同的第二个别名。再放两个同名的数据目录技能，确认两者都按 `skill_<hash>` 列出，重复运行结果相同。

### N1. 运行中会话的 MCP 与技能重载（I54）

按顺序运行（本地 stdio fixture 需要 Node；Worker 使用 loopback 模型端点）：

```sh
cargo test --locked -p deepseek-code-core --lib live_reconcile_preserves_processes_and_drains_calls
cargo test --locked -p deepseek-code-cli --test session_worker running_session_reloads_extensions_on_the_next_request
```

- Linux stdio 生命周期：禁用、重启和退出时按启动器进程树清理，包括已观察到的换组或另建会话的后代。若 daemon 在首次观察前就双重 fork 并被 init 收养，可能仍然存活。Worker 意外退出时由专用 subreaper 回收孤儿。
- 管理器用例：保留未修改服务的 PID，增删、修改、禁用和重新启用另一服务。待移除服务已有调用时，先让调用正常返回再移除。无效配置不能停止现有进程。
- 同一 Worker 连续提交五轮：增加服务和项目技能，修改同名工具定义及技能正文，切换为按需加载，再禁用两者。核对实际请求中的 `tools`、最新用户消息的差异提醒以及保持不变的第一条用户消息，并检查 Worker 的终端通知事件。
- 手工补充：保持交互终端打开，在同一数据目录的第二个终端用 `dcl mcp add/remove/enable/disable` 改动本地测试服务；也可从会话安装。每次只在下一轮检查变更及简短通知，不需重启会话。用编辑器的原子保存方式修改数据目录技能、项目 `.claude/commands`，切换内置技能状态；核对斜杠列表、实际调用内容及模型可见性。项目 `.mcp.json` 不是独立 CLI 自动执行的服务清单，需先按原流程导入。

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

### O2. 内置 task-template

1. `cargo test --locked -p deepseek-code-core --lib skills::bundled::tests::task_template_` 应运行三项测试。用共享解析器检查 YAML frontmatter，验证发现、渲染和只读内置清单，共用的 `SKILL.md` 不落盘，同名用户技能仍优先。清单和展开正文只在决定把实质性工作交给 `Agent`、队友或后台代理时推荐此技能，并明确略过普通后台 shell 命令。
2. 内置技能默认关闭。全新数据目录的第一个请求中，`Available skills` 只应包含默认启用的 `skill-creator`（装了 CodeGraph 时再加 `codegraph`），不应包含 `task-template`。先执行 `dcl skills state task-template user`，再用本地模拟接口启动 `dcl`：`Available skills` 应列出一个 `task-template`，描述包含 `Agent`、队友和后台代理。模拟接口发起 `Skill` 调用后，第二个请求的工具结果应包含传入的任务参数和四段说明（问题、具体改动、验证方式、报告），并要求按 `requirement → evidence → unverified` 记录。
3. 在同一数据目录执行上面的启用命令后，在真实终端输入 `/task-template <任务>`，确认本地模拟接口收到展开的任务参数及同一套四段说明。
4. `cargo test --locked -p deepseek-code-cli --test skills` 覆盖全新数据目录的 `dcl skills list` 和 `dcl skills help task-template`。即使默认关闭，`list` 仍列出该条目并把 `state` 标为 `off`，`help` 仍可读取内容；启用后 `state` 变为 `user`。Desktop 自定义技能清单走同一个 `SkillsManager`。
5. `cargo test --locked -p deepseek-code-cli --test sdk sdk_agent_fields_reach_the_child_request -- --exact` 覆盖 SDK `initialize.agents[].skills`。OpenAI 和 Anthropic 两个本地模拟协议均接受 `task-template` 并把说明预载到子代理请求中。

### O3. 内置技能默认状态与安装速度

1. 全新数据目录中只有 `skill-creator` 默认启用；`codegraph` 仅当 `PATH` 上能找到可执行的 `codegraph`（Windows 也接受 `codegraph.cmd` / `codegraph.exe`）时默认启用；其余内置技能在 `dcl skills list` 中以 `state: "off"` 出现，但不在模型的 `Available skills` 中，模型调用被拒绝。用户可通过 `dcl skills state <NAME> user|off`、`/plugins` 面板或 Desktop 面板启用。
2. 用户的显式选择写入 `<数据目录>/skill-states.json`，升级或重新安装 `<数据目录>/skills/bundled` 后保持不变；删除内置文件后再次运行任何列出/开始会话命令会重新安装它们，但不会重置状态。
3. 安装器先用临时文件加 rename 写完全部文件，再对每个发生变化的目录 fsync 一次，最后原子写安装记录（记录文件与目录单独 fsync）。`cargo test --locked -p deepseek-code-core --lib skills::bundled_install` 覆盖首次安装、同版本空操作、升级保留用户编辑、删除未改动文件和二进制资源；自动测试不能代替真实磁盘上的耗时测量。

### 不经模型的 /doctor 子命令

在有一个格式错误 `SKILL.md` 的工作区（例如 `.claude/skills/bad/SKILL.md` 的 frontmatter 不是有效 YAML）启动 `dcl`，并用本地模拟接口记录模型请求数：

1. 输入 `/doctor errors`：输出列出该文件的 `skill definition:` 与 `definition skipped`，末尾说明可以用 `/doctor` 让模型协助；期间不产生新的模型请求。
2. 输入 `/doctor skills`：输出与 `/skills` 的 Stats 选项卡相同（每个技能的上下文开销、7 天 token、使用次数、最近使用），同样不产生模型请求；MCP 状态与开销已移到 `/tools` 的 Mcp 标签。
3. 输入 `/doctor skills now` 或 `/doctor prompt-audit`：仍交给 `doctor` skill 并调用模型，确认只有精确的 `/doctor skills` 和 `/doctor errors` 走本地路径。

### 第十一轮：SDK 回合内引导（mid-turn steering）

stream-json 宿主在一轮进行中发送的普通用户消息不再只等这一轮结束，而是通过 `services/input_queue` 的 Steer 模式在下一个工具边界交给正在运行的回合；REPL 与 Desktop 的既有路径不变。全部使用本地模拟模型与临时数据目录。

- `cargo test --locked -p deepseek-code-cli --test sdk steering::`：6 项。工具运行中发送第二条消息后，第二条消息出现在工具结果之后的第二个模型请求里，回合继续；两条消息按到达顺序各出现一次；模型直接给出最终回答时该消息作为下一轮恰好运行一次；`/status` 等斜杠命令不进入运行中的回合；`interrupt` 保留尚未交付的消息并在下一轮运行；`--replay-user-messages` 在交付时以原 `uuid` 回显用户信封，且交付事件、持久化消息各一次。
- `cargo test --locked -p deepseek-code-core --lib services::input_queue`：包含 `undelivered_steer_items_are_cancelled_and_delivered_ones_stay`，确认未交付项被取消并返回、已 handoff 项保留。
- `cargo test --locked -p deepseek-code-core --lib durable_steer_handoff_waits_for_boundary_and_pause_preserves_next_turn`：原有边界与暂停语义不变。
- `cargo test --locked -p deepseek-code-cli --test sdk`：包含 `controls::running_turn_answers_queries_and_cancels_only_queued_messages`（运行中发消息后 `cancel_async_message` 取消尚未开始的引导消息）与 `hook_callbacks::sdk_eof_during_model_or_callback_cannot_continue_automatically_allowed_write`（宿主断开且仍需 Hook/MCP 回调时，排队消息连同引导消息一起丢弃，只发一个错误 `result`）。
- 真实模型（本地不接受真实密钥的步骤不在此列）：用 `--input-format stream-json` 保持 stdin 打开，先发送要求三次 `sleep` 工具调用的消息，8 秒后发送“跳过剩余 sleep”的消息。预期 `queued_input` 事件在第一次工具结果之后出现，第二个模型请求包含该文本，剩余调用可以不再执行。2026-11-10 的基线与本地模拟证据在相邻开发支持目录 `steer-20261110/`。

事件形状（与 REPL/Desktop 共用的 `queued_input` 行）：`{"type":"queued_input","text":"…","content":"…","origin":"queue-input-<id>"}`。`origin` 是持久队列项的 handoff 标识；引导消息自己的 `messages` 行已由 handoff 保存，故不再作为工具附件重复写入。REPL/Desktop 的 legacy 队列项保持原有 `origin`（`user`、`task_notification`）并继续依附最近工具结果。

### 第十二轮：失败回合后的 SDK 继续输入与单次 --print

模型流在“已输出正文后静默”时以流超时失败，但失败回合既不结束仍打开的 SDK 输入，也不改变单次 `--print` 的退出约定。全部使用本机回环 HTTP、假密钥与临时项目/数据目录；空闲超时使用 cwd 中正的 `[http].request_timeout_secs = 1`，不是 300 秒生产值。

- `cargo test --locked -p deepseek-code-cli --test api_failure_diagnostics post_content_recovery_vetoes_top_level_and_nested_provider_errors`：直连 Chat 开启后，嵌套 `error` 与顶层 `{"type":"error"}` 都在正文真的空闲超时时各只有 1 次 POST、无提示、一个失败 `stream_error` 结果、退出 1、保留部分正文；等待中的成功回复（唯一标记）未被使用。
- `cargo test --locked -p deepseek-code-cli --test api_failure_diagnostics post_content_recovery_counts_content_on_non_leading_choices`：非首个 choice 上的正文、思考或未完成工具增量都能触发重放（各 2 次 POST、提示指向被放弃的请求、两张快照互相链接、1 次 `message_start`、completed）；被放弃的内容不出现在输出、替换请求体或已存助手正文中，也没有孤儿工具行或文件。整帧只有角色或空增量时仍走 A 的内容前重试：同一 request ID、`retry_wait` 存在、无 C 提示。
- `cargo test --locked -p deepseek-code-cli --test api_failure_diagnostics post_content_disabled_matches_pre_c_snapshot_keys_and_stop_reason`：直连 Chat 省略该键与显式 `false` 时，第一步给出 `stop` 与工具调用、第二步只给正文与原始 `[DONE]`：2 次 POST、marker 为 `one`、结果 `stop_reason:"stop"`、正文 `Before \n\nAfter`；`message_trajectory.requests_json` 的键集与第 13 轮对照一致（第一步 41 键，第二步加 `responseModel`），两个恢复专用键都不存在。
- `cargo test --locked -p deepseek-code-core --test post_content_recovery post_content_recovery_receiver_close_cancels_backoff_open_and_read`：三个独立子进程分别在退避等待、替换响应头前、替换正文读取中关闭事件接收端：执行器都返回 `interrupted`、返回文本为空，且在各自的 watchdog 内返回（远小于 5 秒退避 / 3 秒空闲超时）；物理 POST 分别为 1 / 2 / 2，任何未授权 POST 都不会被等待中的成功回复消费，未完成的 Write 不执行。
- `cargo test --locked -p deepseek-code-cli --test sdk model_stream_timeout_keeps_sdk_stdin_live_after_failed_result`：真实 SDK 子进程（`--input-format stream-json`，工作目录和数据目录都在临时项目中，stderr 被捕获）。stdin 顺序固定：
  1. 只写第一条普通用户帧，stdin 保持打开；
  2. 服务端在 1 次 POST 中刷新 HTTP 200 并发出一个文本增量，随后不再发送任何字节；
  3. 客户端按空闲超时失败，读到 1 个 `result`（`is_error=true`、`stop_reason=stream_error`）；该回合 1 个终止 `turn_timing`（允许此前 1 个 running 帧）、1 个 `request_turn_end`，按 turn ID 与 SQLite 的 failed 行对齐；
  4. 读到失败 `result` 之后才写第二条普通用户帧，仍使用同一进程和同一会话；
  5. 第二次 POST 返回完整回答，第二个 `result` 成功、`session_id` 不变，SQLite 出现 completed 行，两个回合各 1 个 `result`；
  6. 最后才关闭 stdin；只有此时才出现一次 “One or more SDK turns failed or were interrupted”，进程退出 1。
- `cargo test --locked -p deepseek-code-cli --test api_failure_diagnostics single_print_model_stream_timeout_fails_once`：单次 `--print --output-format stream-json` 的同一超时形态：1 次 POST、退出 1、1 个 `stream_error` 结果、部分正文一次、1 个 failed 回合与 1 条终止记录；随后 `--resume` 同一会话对成功回复运行，原 failed 行保留、新回合 completed、退出 0，只新增 1 次 POST。
- `cargo test --locked -p deepseek-code-cli --test api_failure_diagnostics stream_timeout_uses_idle_configuration_for_all_model_transports`：退避取消子用例改用同文件的后台任务取消夹具：先轮询隔离的尝试日志直到出现 `retry_wait`，再写 `cancel` 文件，仍断言退出非 0，且超过计划的重试时刻后只有 1 次 POST；不再依赖 `kill -INT` 和固定 sleep。

自动化只覆盖本机回环与 Linux 进程，不代表 Windows/ConPTY、Desktop 运行时或真实提供商。

### 第十三轮：内容后超时的一次可选重放

`[http].post_content_timeout_recovery` 默认 `false`；开启后仅内置与直连 Chat Completions 生效，整个回合最多一次。全部使用本机回环 HTTP、假密钥、临时项目与数据目录，空闲超时用 cwd 中正的 `[http].request_timeout_secs`（1 秒或 5 秒），退避用毫秒级配置。

- `cargo test --locked -p deepseek-code-cli --test api_failure_diagnostics post_content_timeout_replays_only_the_uncommitted_model_step`：内置 OpenCode Go 与直连 Chat 各跑一次。第 1 个 POST 由模型请求一个经批准的本地 Bash 工具（`printf one >> marker.txt`）并提交；第 2 个 POST 发出思考、正文与一个未完成的 Write，然后保持正文打开直到空闲超时；第 3 个 POST 是替换，返回完整回答。断言：共 3 次 POST；marker 恰为 `one`（工具只运行一次），未完成的 Write 不产生 `orphan.txt`、也不留工具行；最终回答只含替换文本，`ABANDONED_*` 不出现在任何输出；两条被记录的请求体 `messages` 完全相同，且都含先前工具调用/结果与本轮用户输入；尝试日志中恰好 1 条 `post_content_retry`、没有 `retry_wait`，该提示在结果帧之前且只出现一次；该步骤的两张快照通过 `replacesRequestId` 互相链接，被放弃的一张带 `abandonedForRecovery=true`，用量分别为 55/111 的输入 token，结果 `usage` 合计 176/12（按快照 ID 分别记账，不推测被放弃响应的用量）；回合终态 completed、只有 1 条 `request_turn_end`。
- `cargo test --locked -p deepseek-code-cli --test api_failure_diagnostics post_content_timeout_recovery_is_bounded_and_cancellable`：六个子用例——(1) 替换自身在输出后再次停滞：共 2 次 POST，只提交第二次尝试的文本，回合 failed；(2) 替换在响应头前失败：共 2 次 POST，不再重放；(3) 成功重放后下一步骤再超时：共 3 次 POST、只 1 条提示，已提交工具仍只运行一次；(4) 首段已用 `max_retries=1` 把预算花在内容前重试上：共 2 次 POST、无提示；(5) 在提示之后、替换之前写后台任务 `cancel`：超过退避时限仍只有 1 次 POST，1 个终止 `result`，回合为 cancelled；(6) 在替换正文打开时取消：仍为 2 次 POST，未完成的 Write 不执行。
- `cargo test --locked -p deepseek-code-cli --test api_failure_diagnostics post_content_timeout_without_opt_in_retains_the_existing_partial_result`：内置 Chat 分别省略该键与显式 `false`。正文保持打开时即可读到 `VISIBLE_PARTIAL` 增量（早于 `result` 帧），最终 1 次 POST、无 `post_content_retry`、`is_error=true`、`stop_reason=stream_error`、部分正文只出现一次、无 `orphan.txt`、恰好 1 个 failed 回合、退出 1。
- `cargo test --locked -p deepseek-code-cli --test api_failure_diagnostics post_content_recovery_does_not_retry_non_timeout_stream_failures`：开启后四个非超时/终止形态各 1 次 POST 且无提示——文本后干净 EOF（无超时）；内置 Chat 文本 + 原始 `[DONE]` 后正文超时；直连 Chat 文本 + 提供方 SSE 错误后正文超时；`finish_reason` 只出现在非首个 choice 后正文超时。每种都保留文本并给出 1 个失败结果。
- `cargo test --locked -p deepseek-code-cli --test sdk model_stream_timeout_keeps_sdk_stdin_live_after_failed_result` 与 `cargo test --locked -p deepseek-code-cli --test api_failure_diagnostics single_print_model_stream_timeout_fails_once`：默认（关闭）配置下重跑，SDK 的 stdin 继续输入与单次 `--print` 的退出 1/`--resume` 约定不变。

未覆盖：Desktop 运行时、真实外部提供商、Windows/ConPTY，以及启用期间的实时增量（开启时该步骤的文本与思考在步骤完成时才出现）。

## ApplyPatch 补丁工具

`ApplyPatch` 是模型可见的写工具，解析、只读预检和写盘回滚三个内部模块位于 `crates/core/src/tools/apply_patch*.rs`。改动它们或改动 `tools/mod.rs` 的注册/分发时，至少运行：

```bash
env -u DEEPSEEK_CODE_DATA_DIR -u DEEPSEEK_CODE_BACKGROUND_CONTROL -u DEEPSEEK_CODE_BACKGROUND_JOB \
  cargo test --locked -p deepseek-code-core --lib apply_patch
env -u DEEPSEEK_CODE_DATA_DIR -u DEEPSEEK_CODE_BACKGROUND_CONTROL -u DEEPSEEK_CODE_BACKGROUND_JOB \
  cargo test --locked -p deepseek-code-core --lib model_tool_output
env -u DEEPSEEK_CODE_DATA_DIR -u DEEPSEEK_CODE_BACKGROUND_CONTROL -u DEEPSEEK_CODE_BACKGROUND_JOB \
  cargo test --locked -p deepseek-code-cli --test apply_patch_tool
env -u DEEPSEEK_CODE_DATA_DIR -u DEEPSEEK_CODE_BACKGROUND_CONTROL -u DEEPSEEK_CODE_BACKGROUND_JOB \
  cargo test --locked -p deepseek-code-cli --test sdk apply_patch_permissions
```

`crates/cli/tests/apply_patch_tool.rs` 用本地 OpenAI mock 和真实 CLI 覆盖：多文件混合补丁的最终字节与持久化工具记录、第二个文件不匹配时全部不变、工作区外路径与移动目标被拒绝、plan 模式拒绝、Hook 拒绝和 Hook 改写输入后重新校验、ToolLoad 首次发现且只加载一次、无改动补丁与既有 `Edit` 仍然可用，以及 accept-edits 下持久化的 `Edit`/`Write`/`MultiEdit` 路径规则：`Confirm` 与 `Edit` 一致地阻止写入、`Deny` 阻止整补丁、移动目标也参与判断、Hook 把补丁改写到确认/拒绝目标时不会继承批准。核心测试直接驱动引擎审批流程（批准、拒绝、不产生第二次请求）并覆盖 `Confirm` 永不静默变成 `Allow`。测试的进程等待必须使用异步方式（`tokio::process::Command`），默认 current-thread 运行时下同步 `Command::output` 会和服务端一起死锁；mock 每个响应使用不同的调用 ID，避免和调用 ID 复用问题混在一起。直接调用 `ToolLoopExecutor::execute_tool_call` 的单元测试必须像 `execute_scoped`/`run_builtin` 一样用 `path_scope::scope`、`with_permissions` 和 `with_edit_session` 包住调用，否则路径规则的任务局部上下文不存在。

`crates/cli/tests/sdk/apply_patch_permissions.rs` 补充真实 SDK 控制协议层：真实 `dcl` 进程、本地 OpenAI mock、`--input-format stream-json`。测试先用 `SessionStore::create_with_id` 建立 Code 会话，把 `Edit(secret.txt)=Confirm` 写入 `peripheral_settings` 的 `cli.permissions.<id>` 并读回校验，再用 `--resume` 恢复同一会话，权限模式为 accept-edits，同时通过 `support/tool_loading.rs` 让 `ApplyPatch` 常驻加载，避免“工具未加载”伪装成权限保护。覆盖四种情况：Allow 前断言两个目标字节未变且只有一个 `can_use_tool` 请求（工具名为 `ApplyPatch`、输入是生效补丁、带请求 ID），批准后字节精确写为 `ONE`/`LEAK`、只有一次工具结果和一条持久化记录、没有第二次审批、正常结束；Deny 时混合补丁的所有文件不变，结果记录 `permission_denials`，工具结果和持久化记录都是错误；等待真实请求时关闭 SDK 输入，进程在既有 20 秒测试时限内以失败状态退出、不写文件、不留下仍在运行的所属进程；PreToolUse Hook 把允许目标的补丁改写为确认目标并返回 allow 时，审批请求针对改写后的生效补丁，拒绝后原目标和改写目标都不变。这些是本地 mock 的 SDK 协议验证，不是真实远程模型或 Desktop/TUI 验收；SDK 模式下的工具结果出现在 `user` 帧的 `tool_result` 内容块中，持久化记录仍从隔离数据库读取。若把 Hook 用例改成不恢复带规则的会话，测试会因缺少显式规则而走自动放行；该失败是 fixture 错误，不是产品绕过证据。

工具调用身份采用“每次执行一个 occurrence id”的约定：事件、显示编号、`permission_denials`、`tool_result`、SDK `can_use_tool.tool_use_id`、hook payload 和持久化行都使用 `inv-<turn-scope>-<n>`；供应商原始调用 ID 只用于出站协议配对。因此 SDK 测试断言的是实际观察到的 occurrence id（非空、`inv-` 前缀、不等于 mock 的原始 `apply-1`），并单独在 provider 请求体中确认 `tool_calls[].id` 与 `tool_call_id` 仍是同一条原始 ID；持久化行 id 以 `:<occurrence>` 结尾。`sdk_reused_provider_id_keeps_occurrences_prompts_and_records_independent` 覆盖同一供应商 ID 被连续复用两次：两次补丁目标都处于 `Confirm`，第一次批准后只写入 `a.txt`，第二次仍单独弹出审批请求（request id 和 occurrence id 都与第一次不同），拒绝后 `secret.txt` 保持不变，两条持久化记录一条成功一条失败，provider 侧两次 `tool_call_id` 仍是同一个原始 ID。不要把该用例改成依赖自动放行来“通过”。

SDK 审批等待期间关闭输入或发送 interrupt 时，还要核对持久化终态，不能只检查进程退出和文件不变：

- `cargo test --locked -p deepseek-code-cli --test sdk apply_patch_permissions`：包含原 `sdk_input_closed_while_the_patch_waits_for_approval_never_writes`；准确 occurrence 对应且恰好一条工具行，`is_error=true`、`output` 为既有 `UNFINISHED_TOOL_RESULT`，关联回合为 cancelled，补丁目标字节不变。
- `cargo test --locked -p deepseek-code-cli --test sdk approval_cancellation`：Write/Bash × EOF/interrupt 四个用例，每个覆盖 OpenAI/Anthropic；等待真实 `can_use_tool` 后取消，无目标文件、无多余模型请求，interrupt 后同 session 下一轮正常完成，旧调用终态不被改写。此文件是 SDK 子模块，没有独立的 `--test apply_patch_permissions` 目标。
- `cargo test --locked -p deepseek-code-cli --bin dcl -- background_transcript_tests compaction_cancellation_tests`：先收齐已报告结果，再仅对缺少结果的工具补错误终态；已有成功（包括空输出）、失败与队列中晚到的完成结果保持，重复结算不改值。`SDK turn interrupted` 归为取消，不扩宽普通提供商/Hook 错误的分类。
- 同时运行 SDK 全套及 core 的 `tool_approval::tests::`、`tool_loading_tests::`、`executor_lifetime_tests::`、`child_transcript::tests::`；分别覆盖审批决策、审批/运行取消、执行器与强制写库尾部的所有权、子会话最后事件收集。模型入口、核心 mailbox 取消及 Desktop 运行是不同证据层，不能互相替代。

公共收尾使用结果未知的既有提示，不声称正在执行的工具没有副作用或已回滚；只有审批测试通过文件/marker 断言证明没有执行。真实完成结果必须优先于取消时的兜底结果。

`ApplyPatch` 只做进程内失败一致性：受控失败会回滚已写文件，但进程崩溃没有日志恢复，不能当作跨文件原子操作验收。Linux 上的单元与 CLI 证据不代表 Windows 的 ACL/只读属性行为。

## ToolScript 嵌入式 JS 基础（J1–J3）

`crates/core/src/toolscript/` 是 ToolScript 的引擎基础，使用锁定版本 `rquickjs 0.14.0` 在进程内执行真实 QuickJS。它目前只暴露 Rust API 和 `tools.call` 的 fake callback 测试，**没有**注册模型可见工具，也没有接入 `ToolLoopExecutor` 的权限、Hook、历史、事件或持久化。完整契约与限制见 `docs/toolscript-foundation.md`。

改动该目录或 `crates/core/Cargo.toml` 的 `rquickjs` 依赖后运行：

```bash
cargo test --locked -p deepseek-code-core --lib toolscript::
cargo check --locked -p deepseek-code-core --lib
cargo check --locked -p deepseek-code-cli
cargo check --locked -p deepseek-code-desktop --all-targets
git diff --check
```

33 个用例使用真实 QuickJS 和 fake host callback，覆盖：顺序调用、`Promise.all` 顺序与峰值并发、条件/变换、结构化工具错误与脚本抛出、Unicode、非 JSON/循环参数、脚本结果限制、CPU 死循环的 QuickJS interrupt hook、外部线程取消、栈溢出、内存压力、等待中的 host future 取消/超时、脚本结束时未 await 调用的 Drop 与无迟到副作用、日志/参数/结果/并发/待定调用上限，以及 `require`/`process`/`fetch`/timer 全局不存在。以下回归专门证明本轮边界：

- `private_prelude_bindings_are_not_visible_to_user_code`：用户 body 与 prelude 分属两个脚本，`rawCall`、`config`、`normalizeResult`、内部 globals 都是 `undefined`。
- `monkeypatched_intrinsics_cannot_change_arguments_or_bypass_validation`：改写 `JSON.stringify`、`JSON.parse`、`Object.keys`、`Object.getPrototypeOf`、`Number.isFinite`、`Array.isArray` 后，Rust 仍收到原始参数并拒绝循环值。
- `function_constructor_and_dynamic_import_expose_no_host_or_io`：`Function` 构造函数看不到 host helper；没有 module loader/动态 import 的 I/O 能力。
- `cancellation_during_a_waiting_host_call_drops_the_future`、`host_call_timeout_drops_the_future`、`unawaited_call_is_abandoned_without_a_late_effect`：断言真实 host `Drop` guard，而不只是计数器，并在返回后等待确认没有迟到副作用。
- `numeric_boundary_preserves_values_in_arguments`、`numeric_boundary_preserves_values_in_script_results`：覆盖 `±2^53`、`i64` 边界、`2^63`、`2^64`、`1e20`、`-0` 与分数字面量。`2^63` 必须保持浮点值，不能被 `i64::MAX as f64` 饱和成 `9223372036854775807`；旧代码负向复现见 `round02-negative-numeric-02.log`。
- `sparse_array_holes_are_rejected_despite_inherited_values`：`Array.prototype[0]=7` 加 `new Array(1)` 与 `[1, , 3]` 都被拒绝，dense 数组通过；旧代码把 hole 读成继承的 `[7]`，负向复现见 `round02-negative-sparse-01.log`。
- `getter_mutation_cannot_resurrect_an_argument_hole_from_the_prototype`、`getter_mutation_cannot_resurrect_a_result_hole_from_the_prototype`：index 0 的 getter 删除 index 1 后，边界在每次读取前重新做 native own-property 检查，不会回退到 `Array.prototype[1]`；参数场景要求 host 未收到调用。旧代码负向复现见 `round02-negative-getter-01.log`。
- `proxy_values_are_rejected_before_traps_or_enumeration`：数组和对象 proxy 都在结构遍历前按 class id 拒绝，`get`/`ownKeys`/`getOwnPropertyDescriptor` trap 计数为 0。
- `out_of_range_numeric_property_on_an_array_is_ignored`：`"4294967295"`（`2^32-1`）不是数组索引，只是普通自有属性，不当作 hole；负向复现见 `round02-negative-indexrange-01.log`。

数组 own-property 检查使用 QuickJS 的 native `JS_GetOwnProperty`（空 descriptor），不经过 `in`、`Object.keys` 或任何可被脚本替换的 intrinsic；边界仍会先做一次 own-key 连续性扫描，再在每次读取前逐 index 复查。这些用例只证明 JS 引擎、边界和限制，不证明权限、Hook、历史、持久化或真实执行器分发。Android（返回 `UnsupportedPlatform`，未构建）、Windows、macOS 未验证；阻塞式非 async host handler 不能被抢占；`run` 是 `!Send`，集成时必须由专用 current-thread executor 持有 VM。

## ToolScript 执行器桥接（J4，引擎内部）

`crates/core/src/native_engine/toolscript_bridge.rs` 是 `ToolLoopExecutor::run_toolscript` 的内部入口：真实 QuickJS 脚本的 `tools.call` 走现有 `prepare_tool_call_with`/`complete_tool_call_with`（加载策略、实时权限模式、Plan reset、Pre/Post hook、真实权限提示、路径/文件状态/MCP/sandbox），不注册模型可见工具，也没有持久后台任务生命周期。VM 在专用 16 MiB 线程的 current-thread runtime 上运行，只通过有界 channel 与执行器交换 JSON/id，不使用 `unsafe Send/Sync`。完整契约见 `docs/toolscript-executor-bridge.md`。

改动该目录、`services/tool_history.rs`、`tool_call_origins` schema、`SessionStore::model_history` 或 CLI `BackgroundTranscript` 后运行：

```bash
cargo test -p deepseek-code-core --lib toolscript_bridge
cargo test -p deepseek-code-core --lib "toolscript::"
cargo test -p deepseek-code-core --lib "services::tool_history"
cargo test -p deepseek-code-core --lib "db::conversation_repo"
cargo test -p deepseek-code-cli --bins background_transcript
cargo check -p deepseek-code-core --lib
cargo check -p deepseek-code-cli
cargo check -p deepseek-code-desktop --all-targets
```

17 个用例（15 个集成用例 + 2 个 `toolscript_bridge` 内部单测）使用真实 QuickJS、真实 Read/Bash/Edit 和真实 SQLite：

- `sequential_calls_execute_real_tools_and_persist_after_reopen`：值/文件正确；事件顺序为 origin 先于 start；经共享 writer 持久化并重开后 4 行、独立 `inv-` id、parent/sequence 正确；OpenAI 与 Anthropic `model_history` 都不含嵌套 id。
- `repeated_calls_and_replayed_events_keep_distinct_rows`：相同工具/参数两次调用保留两行；重复事件不覆盖。
- `plan_mode_denies_edit_without_a_file_effect`、`unloaded_and_lifecycle_tools_are_refused_before_side_effects`、`pre_hook_blocks_and_rewrites_nested_input`：拒绝/未加载/生命周期工具/Pre-hook block 均无副作用且有终态记录；`ToolUseDone` 输入为 hook 后的有效输入。
- `live_policy_changes_apply_between_script_calls`：两次脚本之间 PlanMode 生效。
- `script_error_propagates_and_history_keeps_the_terminal_outcome`：脚本异常保留已执行工具的真实结果。
- `cancellation_during_bash_terminates_the_exact_owned_processes`：fixture 先发布 shell 与后台子进程的 PID 及启动时间，取消后等两个精确身份消失，并等待超过命令自身副作用时长的 horizon；失败路径只用精确 PID 清理，不发宽泛信号。`ProcessTree::drop` 只发送 `SIGKILL(-pgid)`，不等待内核回收，这一点在文档中如实声明。
- `dropping_the_alive_receiver_stops_a_started_bash_body`（单测）：这是**私有 per-call alive 缝**，与全局 `Cancellation` 不同：真实 Bash body 启动并发布 PID 后直接 drop alive sender，验证已启动 body 被丢弃且精确进程消失。清理 guard 在 barrier 读到 PID+启动时间时立即构造、经 `tokio::join!` 返回并在最终断言前一直持有，`join` 另一分支 panic 时仍会 drop；不重写原有显式取消用例。
- `settling_script_abandons_an_unawaited_call_before_dispatch`：脚本在最后一次 await 后立即结束时，未 await 的调用不会被提交（`calls_started == 0`）。
- `a_permission_wait_aborts_when_the_script_settles_early`：调用确实进入权限等待（观察到 `ToolPermission`）后脚本结束，准备阶段立即放弃而不是等到 600 s。
- `completed_sibling_results_survive_cancellation`：取消时已完成的安全调用保留成功记录，阻塞的兄弟标记为取消。
- `a_cancelled_first_batch_records_every_later_admitted_call`：被取消的首批 Bash、排队的 Write/Read 都得到终态记录；`late.txt` 不存在；持久化 id 完整且被重放排除。
- `a_hook_rewritten_background_bash_is_refused_before_the_body`：hook 把 Bash 改写为 `run_in_background` 后仍在 body 前拒绝，无 sentinel、无 shell task 目录。
- `a_hook_rewritten_mutation_does_not_overlap_a_safe_neighbor`：FIFO handshake 证明 hook 改写出的写命令不会与相邻安全调用并发。
- `promise_all_reads_overlap_while_mutations_stay_serial`：FIFO handshake 证明安全读并发、写调用串行。
- `a_prepared_mutation_with_a_gone_caller_never_runs`：body 前已放弃的写调用不会创建文件。

`services::tool_history` 另有 6 个用例：row+origin 原子回滚、origin 先到而 row 未到时延后、冲突 origin 不重新认父、消息删除级联、late delta/start 不覆盖已完成输入、实时事件投影。Android（返回 `UnsupportedPlatform`）、Windows、macOS 未验证；Desktop `src-tauri` 仅转发 `nested_tool_call` 事件，尚未写 origins，且该工具未在 Desktop 注册。无模型可见注册、无 TaskGet/TaskStop、无持久后台脚本。

## ToolScript 持久任务权限刷新（J2b）

`crates/core/src/native_engine/toolscript_job.rs` 的 owner 每 500 ms 重新读取并**严格**应用整份持久权限快照（settings、会话规则、持久模式）。任一必需读取/解析/应用失败都会记录首个错误、取消该脚本并停止再接纳内层调用；终态为 `failed` + `policy_refresh_failed`，不会写成用户 `cancelled` 或 `completed`。此前 `if let Ok` 保留旧 Allow 的行为已删除。

```bash
env -u DEEPSEEK_CODE_BACKGROUND_CONTROL -u DEEPSEEK_CODE_BACKGROUND_JOB \
  cargo test --locked -p deepseek-code-core --lib toolscript_job::j2_tests
cargo test --locked -p deepseek-code-core --lib toolscript_job::j1_tests
cargo build --locked -p deepseek-code-cli
python3 ../dev-support/deepseek-code/toolscript-20261111/j2b/run_j2b.py \
  target/debug/dcl /tmp/j2b-fixtures /tmp/j2b-result.json
```

6 个单测覆盖严格读取层。`toolscript_job::j2_tests` 5 个：损坏的会话规则、损坏的持久模式、存在但损坏的 settings 文件都带阶段失败，缺失文件不算失败；settings 中的 deny 不会被旧存储的 Allow 放宽；被拒绝的规则集不会清空已应用规则。`services::permission_updates::tests::strict_loading_rejects_malformed_permission_fields_the_lenient_path_skips` 1 个：`permissions` 不是对象、`deny` 是数字、`deny` 数组混入非字符串、`allow` 不是数组、`additionalDirectories` 混入非字符串、`defaultMode` 不是字符串、`deny` 规则非法，都在 strict 分支返回带文件名与字段的阶段错误，同时宽松路径对这些输入仍成功加载。

真实 CLI 私有 worker（`DEEPSEEK_CODE_TOOLSCRIPT_JOB`）5 个用例，均使用隔离数据目录、本地 fixture provider（`127.0.0.1:1`，无真实凭据）、ask + 持久 `Bash: Allow`，脚本先完成一次 `Bash` marker，再进入有界真实工具调用循环等待 release 文件：

- 会话规则损坏、持久模式损坏、settings 文件损坏、settings 为合法 JSON 但 `deny` 是数字（`{"permissions":{"deny":7}}`，即父级复现该缺陷的原始形状）：驱动等首个调用 `done` 后注入，再等 worker 自己写出 `policy_refresh_failed` 日志行（该行在请求取消之后写入），然后创建 release。断言终态 `failed`/`policy_refresh_failed`、错误阶段准确（结构倒例为 `permissions.deny must be an array of strings`）、`result.json` 的 `policy_refresh_failed=true` 且 `cancelled=false`、任务记录 `failed`、第一个 marker 存在、第二个 marker 不存在、`output.log` 保留已完成的调用记录、`calls_started`/`calls_succeeded` 为真实计数；恢复被破坏的来源后重跑同一 worker 必须被“terminal state”拒绝且 `state.json` 字节不变。
- 运行中把持久规则改为 `Bash: Deny`：脚本捕获 `permission_denied` 后返回，终态 `completed`、`calls_succeeded=1`、第二个 marker 不存在，证明撤销在下一个调用生效。

修复前的失败证据保留在 `supervision-20261110/restart-review-20261112/j2b-malformed-probe.json`（父级用同一 CLI 复现：`{"permissions":{"deny":7}}` 时 marker2 产生且 job `completed`），本轮的通过结果不能替代它。

口径说明：

- 500 ms 周期刷新**不是**每次调用即时读取的保证：同一窗口内已接纳的调用可能仍按旧策略执行。本场景只证明“刷新失败即停止并在下一调用前拒绝”，不证明所有权限边界即时一致。
- 失败被检测前已经开始的并发批次按实际结果记录，不声称这些已发生的副作用被撤销。
- 已执行调用保留在 `output.log` 与历史行中；`state.calls_*` 由同一事件流统计（脚本已返回时用引擎统计）。脚本在一次失败刷新同一 tick 内已返回的窄竞争会被记为 `failed`，其值保留在 `result.json` 的 `script_value`。
- 未被覆盖的边界另行记录：每次调用的即时权限读取、等待审批期间撤销权限、运行中的 hooks 变化、J3 内层持久化、J4 真实审批、J5 停止清理。
- 宽松的 CLI 交互路径（`load_settings`）不变；严格版本只在 ToolScript job 内使用。产品自身的 settings 写入是 temp+rename，不会产生半写文件。

## ToolScript 真实 CLI 历史闭环（J3a）

```bash
cargo test --locked -j 2 -p deepseek-code-cli --test toolscript_history -- --nocapture
```

`cli_toolscript_repeated_calls_keep_history_across_model_rounds_and_reopen` 使用当前构建的真实 `dcl` 与它启动的真实 ToolScript worker、本地 OpenAI Chat Completions mock、临时数据目录。ToolScript 默认按需加载；fixture 不覆盖其加载配置，断言首请求没有该工具，模型先调用 `ToolLoad({"names":["ToolScript"]})`，成功后下一请求才包含它。fixture 不创建会话、assistant 或工具记录。

两个模型轮次故意复用 `same-provider-id` 作为 ToolScript raw provider ID。每个脚本先用真实 Bash 写本轮标记与 `same.txt`，再用完全相同的参数执行两次顺序 Read、三次 `Promise.all` Read，最后尝试被显式 deny 的 Write 并捕获拒绝。每个脚本 7 次实际调用（6 成功、1 拒绝）；逐个核对 worker log 的 start/done/origin 与数据库的 invocation ID、assistant ID、parent ID、sequence、原参数和完整结果。第二轮改变文件内容，第一轮结果必须仍为 ROUND_1，第二轮为 ROUND_2，拒绝目标文件不存在。

验收要求：14 条独立内层记录、2 条不同的 ToolScript 父调用、1 条 ToolLoad，共 17 条工具记录；关闭 SQLite 连接再独立重开后全部字段相等，`PRAGMA foreign_key_check` 为零。`--nocapture` 输出实际 CLI 退出码、每条 DB 行、worker spec/state/result/output.log 和重开结论，供提取证据；CLI 退出 0 本身不替代这些断言。

当前 CLI 的 `TurnTimer::ensure_segment`（`turn_repo.rs`）在提交真实 assistant 与 turn 关联后才返回，`session.rs` 随后把该 ID 放入 ChatRequest 并发送模型请求。本用例还通过独立只读 SQLite 连接，确认每次模型响应发出前已有该会话的唯一 assistant。当前分支未复现“父 assistant 尚未落库”的 FK 竞态，因此没有为此添加等待/ack 或改写持久化路径；不能把此前 `assistant_id=None` 的私有 worker fixture 当作本验收。

范围：Linux 的普通前台 CLI 单个用户回合内两次模型工具轮次、OpenAI mock、真实 worker、SQLite 重开。未覆盖 Anthropic/Responses、Windows/macOS、Desktop 运行、真实提供商、Plan reset 后重新关联、后台父进程提前退出、J4 审批、J5 停止、J3b 损坏磁盘处理。旧的基础/桥接章节描述其当阶段的测试边界，当前模型入口的历史验收以本节为准。

### ToolScript 与 TaskGet 的按需加载回归

- 保持 `services::customization::tests::only_core_tools_load_by_default_and_every_tool_stays_discoverable` 和 SDK `sdk_cli_requests_default_core_tools_and_loads_an_on_demand_one` 的默认列表断言；ToolScript 不在 CORE_TOOLS 中，仍可被 ToolLoad 发现。J3a 用例直接验证默认配置的加载与真实执行。
- ToolScript 后台启动、前台等待超时和工具说明都提示先通过 ToolLoad 加载 TaskGet，再读取输出；TaskStop 仍可直接使用。
- `task_input` 的普通、逐次记录和 PreToolUse 拒绝三个 fixture 先调用 `ToolLoad({"names":["TaskInput","TaskGet"]})`，然后保留真实 stdin/EOF、Hook、拒绝无写入和历史断言。
- `taskget_background::cli_task_get_pages_a_background_shell_to_its_exit_status` 专测分页执行，在独立数据目录通过 `support/tool_loading.rs` 显式开启 TaskGet，保留完整分页、字节上限和终态断言；同文件另一用例验证模型通过 `ToolLoad({"names":["TaskGet"]})` 自行加载。
- `sdk` 全文件中的 `apply_patch_permissions::sdk_input_closed_while_the_patch_waits_for_approval_never_writes` 是监督者确认的公共 ApplyPatch 取消问题，交由运行时分支处理，不因本轮默认工具修复放宽断言。

## ToolScript 内层审批桥接（J4）

内层调用的 `RequiresConfirmation` 经 job 内 UUID request/response 文件转交拥有该调用的前端，继续使用普通 `ToolPermission` mailbox、SDK `can_use_tool`、终端审批菜单和 Desktop 审批通道。请求保留实际输入与内层 occurrence ID；`toolscript` 来源包含 `parent_tool_use_id` 和 `task_id`。来源不是工具参数，不改变执行输入。父上下文处理普通审批超时、用户选择、改写输入与规则提交；worker 接收结果后再严格读取持久权限并校验输入，显式 deny 与 Super 门禁不能被批准覆盖。

每个审批文件只属于一个内层调用。一次允许不改变脚本权限；用户明确选择现有持久规则或会话授权时，后续调用按该规则判定。审批宿主持有独立 OS 文件锁，worker 在等候回复时检测宿主消失；TaskStop、脚本取消/结束和宿主关闭都使待定请求失效，取消 mailbox watcher，拒绝迟到回复。worker 的 500 ms 严格刷新在等待期间继续运行，刷新失败仍记为 `policy_refresh_failed`。普通无宿主 `--print` 明确拒绝；SDK 可用 `initialize.canUseTool=false` 明确关闭审批能力，省略时保持原 SDK 默认行为。

```bash
cargo test --locked -j 2 -p deepseek-code-cli --test sdk toolscript_approval -- --nocapture
cargo test --locked -j 2 -p deepseek-code-core --lib -- toolscript services::permission_updates native_engine::tool_loop::tool_approval tool_loading_tests executor_lifetime_tests child_transcript::tests
cargo test --locked -j 2 -p deepseek-code-cli --test toolscript_history --test toolscript_super_agent --test sdk
cargo check --locked -j 2 -p deepseek-code-core -p deepseek-code-cli -p deepseek-code-desktop --all-targets
```

SDK 用例通过真实 CLI、本地 OpenAI mock、ToolLoad 与私有 worker 验证允许/拒绝、两次独立审批、持久规则、批准改写输入、等待时 EOF、真实模型 TaskStop 与迟到 allow、持久 deny 和损坏策略刷新、无审批能力、Router/Coder 门禁优先，以及 PreToolUse 强制 ask 不被普通 allow 规则绕过。测试输出 `J4_APPROVAL`、`J4_STATE`、`J4_LOG`、`J4_DB`，逐项核对调用 ID、父 ID、输入、输出、错误终态和文件副作用，不预造 assistant 或工具行。Super 场景仅预设真实会话角色及任务关系。夹具在 ask 模式只预先允许外层 ToolScript；内层 Write 必须独立审批。TaskStop 场景另允许 TaskStop，以便模型能够取消待审脚本，这不授权 Write。

另外重跑相邻 dev-support 中 `toolscript-20261111/j2a/run_j2a.py` 和 `j2b/run_j2b.py`：先读脚本用法，以本 worktree 构建的 `target/debug/dcl` 和全新临时 fixture 目录执行，不能复用旧证据。J2 脚本只验私有 worker，J4 SDK 用例才验证真实模型入口。

人工终端/ Desktop 验收：ask 模式让 ToolScript 执行 Write，现有审批框应显示原工具预览和 `From ToolScript (parent …)`，分别选择一次允许、拒绝、会话/规则授权；取消脚本后旧提示消失，迟到允许不得写文件。编译、组件测试、SDK 协议测试不能替代真实 TUI / Desktop 运行验收。Windows/macOS、远程提供商、跨回合后台提示接管和磁盘故障须单独报告，不推断为通过。

## 按改动范围选择场景

- `terminal/editor.rs`、`terminal/live.rs` 的按键处理：A、A3、A4、B、E1、E2、E3
- `terminal/live.rs`、`terminal/scrollback.rs`、`terminal/change_view.rs`、`terminal/tool_view.rs` 的显示：B、C、E1
- `repl.rs`：C4、D1、D2、D6、D7、D8–D10（恢复提示、完成耗时与运行中状态），以及 L（手动 `/compact` 的完成输出、提交前取消与 `post-commit-manual` 的提交后取消）
- `terminal/input.rs` 的取消通道：L 的 `cancel-manual`、`cancel-empty-manual`、`cancel-invalid-manual`、`cancel-merge-manual`、`post-commit-manual`，并在真实终端确认 `Esc` 不滞留到下一个提示、等待期间 `Ctrl+O` 仍能打开完整记录
- `status_commands.rs`、`services/context_usage.rs`：`status_commands_tests`、`compaction_status_regressions` 与 L 的 `status-manual`（按边界与配置匹配用量、部分估算、压缩后估算、次数持久化）
- `session_commands.rs`：L（自动压缩完成输出与次数）；模型请求压缩（`services/compaction_requests.rs`、`prepare_turn` 开头的取用）另见“模型请求压缩（CompactContext）”
- `worker_frontend.rs`、`background_commands.rs`：B、D、E3、E4、F；任务状态瞬时读失败另需手工场景：长回合运行中把该 job 的 `state.json` 改名约 200ms，回合必须继续，`stderr.log` 出现 `Task state observation deferred` 与 `recovered`；连续失败超过 5 次或 2 秒时，视图以 `Could not read the task state` 结束而 worker 继续运行（`read_job` 的 5×10ms 重试覆盖单次读，前端 `StateRetry` 覆盖更长的外部干扰）。
- `crates/cli/tests/conpty_worker.rs`、`crates/cli/tests/fixtures/conpty-console.ps1`：见“Windows ConPTY”中的并发启动与进程树清理场景。Linux 仅检查此测试目标可以编译。
- `memory.rs`、`display_commands.rs`、`crates/core/src/memory`：G
- `services/customization.rs`、`prompt/`、`native_engine/tool_loop.rs`：H，以及 SDK 测试
- `tools/python.rs`、`services/python_settings.rs`、`prompt/` 的 Python 环境行、`native_engine/model_tool_output.rs` 的 Python 渲染：P（Python 工具），以及 H（工具加载、权限与自定义）
- `tools/context_search.rs`、`db/context_search_repo.rs`、`db/context_search_index.rs` 与 ContextSearch 的工具注册：H、L 和下文“压缩后检索当前会话（ContextSearch）”。
- `crates/core/src/skills/discovery.rs`、`crates/core/src/native_engine/engine_core.rs` 的技能列表、`crates/core/src/native_engine/skill_tool.rs`：N
- `crates/core/src/skills/mod.rs` 的 bundled 安装、`bundled.rs`、`bundled_resources.rs`、`bundled_manifest.rs`、`skill_creator.rs`、`skills/bundled/`：O（安装与 `ensure_installed` 的旧占位迁移见 N、O 两节）
- `crates/core/src/skills/bundled.rs` 的 task-template 注册或 `skills/bundled/task-template.md`：O2
- `tools/cron_jobs.rs`、`tools/wakeup.rs`、`main.rs` 的空闲循环：I
- `terminal/picker.rs`：F，以及 `/resume`、`/rewind`、`/mcp` 等使用选择列表的命令
- 欢迎区生命周期、输入框与运行帧交接、`/plan` 提示：J，以及 B、C、E1、E3
- `session.rs`：B、D（含 D8–D10）、E；SDK 输入路径另见“第十一轮：SDK 回合内引导”；回合内的执行所有权（含反应式压缩与钩子）按 D8 第 8 条，另跑 `cargo test --locked -p deepseek-code-cli --test live_observation ownership_survives`
- `db/turn_repo.rs`、`services/session_messages.rs` 的回合状态与执行租约、`observe.rs` 的只读观察：D8–D10，以及 `cargo test --locked -p deepseek-code-cli --test live_observation`、`cargo test --locked -p deepseek-code-core --lib db::turn_repo`、`cargo test --locked -p deepseek-code-cli --bin dcl observe::`；恢复/租约回归另跑 `cargo test --locked -p deepseek-code-cli --test session_worker --test session_messages`；`crates/core/src/native_engine/engine_core.rs`、`tool_loop.rs` 的租约字段类型改动另跑 `cargo test --locked -p deepseek-code-core --lib native_engine::`
- `services/session_worker*`、`services/background_sessions.rs`：D、E4，以及 `cargo test --locked -p deepseek-code-core --test worker_environment`（普通子进程不继承 worker 的控制变量）
- `args.rs`、`restart.rs`、`remote_host.rs` 及 `main.rs` 的进程交接，`batch_commands.rs`、`workflow_runner.rs`、`native_engine/script_workflow.rs`、`tools/external_agents.rs` 和 Worker 的模型快照改动：I4。涉及 `session.rs`、`persistent_worker.rs` 时，同时运行 Worker 生命周期与本地协调回归。
- `tools/coordination.rs`、`native_engine/tool_loop.rs`、`native_engine/plan_tool.rs` 的计划入口退役：`cargo test --locked -p deepseek-code-core --lib native_engine::tool_loading_tests`、`cargo test --locked -p deepseek-code-core --lib native_engine::plan_tool::tests`、`cargo test --locked -p deepseek-code-core --lib tools::coordination::tests`（四个任务内置各一次、`TaskOutput` 与 `plan_mode` 不在目录/请求/搜索中；旧设置启用和旧 ToolLoad 记录都不能恢复 `plan_mode`；直接调用报 Unknown tool 且不创建 `.claude`；`EnterPlanMode`/`ExitPlanMode` 审批与计划边界仍通过）
- `tools/plan_import.rs`：`cargo test --locked -p deepseek-code-core --lib tools::plan_import`（显式内部导入：真实 legacy SQLite → 会话任务库，状态/结果/反向依赖保留且重开可读；dry run 与缺失/非法源不创建目标；未知状态/重复步骤/悬空/自引用/成环/超限与中途插入失败整体回滚；同源哈希幂等并保留手工编辑；源变化冲突不写；映射任务缺失报冲突；同一 plan UUID 跨 workspace/session 隔离；并发只建一套；current 标记只在同 workspace/session 内更新；`current_plan` 单独变化（记录 body 不变）两个方向都判为冲突且不写入；不相关计划的 current 切换仍是 unchanged；旧版哈希格式报明确冲突且不重建映射行）
- `crates/cli/tests/tasks_catalog.rs`：`cargo test --locked -p deepseek-code-cli --test tasks_catalog`（真实 CLI + 本地 mock：四个任务工具默认不可见，经两个 ToolLoad 精确名称数组调用后在下一请求出现；核对实际请求 schema，并通过真实任务工具创建根/步骤、设置依赖和输出、列表与读取；持久化行/边/结果与 `TaskGet` 一致，请求中没有 `plan_mode`/`TaskOutput`）
- `crates/cli/src/task_commands.rs`、`args.rs` 的 `tasks import-plan`：`cargo test --locked -p deepseek-code-cli --test tasks_import`（真实 `dcl` 子进程 + 本地临时 legacy/数据目录：成功导入后可用任务存储读回根/步骤/依赖；dry run 与缺失/非法源、非法会话/计划、缺少或同时给出选择器都不创建目标；仅 current 变化返回 conflict 且退出码 1、任务不变；`--data-dir` 隔离；workspace 旧库保持只读）
- `tools/tasks.rs`、`tools/background_shell.rs`、Workflow/背景 Agent 的 TaskGet 输出：`cargo test --locked -p deepseek-code-cli --test taskget_backends --test taskget_background`（真实 CLI + 本地 mock：Workflow 实时输出、完成、超时不取消、TaskStop 取消；背景 Agent 运行快照、报告、退出码、超时不停、会话隔离；请求中四个任务内置各一次且无 TaskOutput，并核对实际请求的 TaskUpdate schema 带 `setBlockedBy`），以及 `cargo test --locked -p deepseek-code-core --lib tools::tasks` 与 `cargo test --locked -p deepseek-code-core --lib tools::background_shell`。`tools::tasks` 覆盖 `TaskUpdate setBlockedBy` 的原子替换/清空、反向 `blocks` 同步、重复项去重、缺失/自引用/成环/跨会话拒绝且失败不落盘、`addBlockedBy`/`addBlocks`/删除混用时先拒绝。
- TaskGet 增量输出页：`cargo test --locked -p deepseek-code-core --lib tools::output_page`、`cargo test --locked -p deepseek-code-core --lib background_shell::foreground_output_tests::task_get_`、`cargo test --locked -p deepseek-code-core --lib tools::tasks::tests`，以及真实 CLI 的 `cargo test --locked -p deepseek-code-cli --test taskget_background cli_task_get_pages_a_background_shell_to_its_exit_status`（真实 `dcl` + 本地 mock：后台 Bash 写出两行 stdout 和一行 stderr，模型用 `max_output_bytes:5` 连续读页直到 `has_more:false`；断言每页 `stdout+stderr` 字节数不超过上限、逐页拼接后与原始输出完全一致、最后一页 `status:"completed"`/`retrieval_status:"success"`、页面中不重复完整的 `output`/`result` 字段）。
  契约：传入 `cursor` 或 `max_output_bytes` 才进入分页模式，否则保持原快照行为。`max_output_bytes` 取 4..1048576；不足 4 直接报错（小于一个 UTF-8 字符无法推进）。`next_cursor` 是不透明、无状态、不含权限的字符串（`tg1.` + hex JSON），绑定任务、后端和每条流的输出代际；两个读者用同一 cursor 互不影响。`has_more`/`output_truncated` 表示本页是否被上限截断，`output_gap` 在出现无法解码的字节时给出 `invalid_utf8` 及字节数。
  代际检测只覆盖：追加（代际不变、继续读）、从未读取过的流稍后创建（合法的空到首输出）、换 inode 的轮转、短于游标位置的截断。**同 inode 原地改写，或截断后重新增长超过保存的偏移，不会被检测**——要检测需由生产者维护代际或重读已消费字节，本实现明确记录该限制，不假装任意改写都能发现。
  语义回归（第 17 轮）：`output_page` 的 metadata-only 页、真正的空 `result`/`delta` 字符串、metadata 跨页后首条文本、以及逐页拼接等于原快照文本；`out=0` 但 `out_skip>0`（首条记录只返回了一部分）也必须拒绝换 inode 的输出，而追加重放和稍后创建的 stderr 仍然合法。后台 job 的 `job_report` 只在任务进入终态后才替换输出：运行中即使子会话已有历史/中间 assistant 消息，`TaskGet` 分页仍来自当前 `output.jsonl`，游标继续有效；运行到完成的跨越游标会明确报错。真实 CLI 用例为 `background_agent_task_get_pages_while_a_prior_assistant_turn_exists`（本地 mock、真实背景 Agent，子会话预置历史 assistant 行；用 fixture gate 保证两页都在 `running` 时读取）。已完成报告仍从 SQLite 整条取出后再在内存中截断，DB 读取不受日志分页 IO 上限约束。
  运行中的文件若停在多字节字符中间，偏移会停在该字符之前等下一次写入，不吞掉也不替换；只有任务已终止时才把末尾残余字节定义为无效并以 U+FFFD 替换，同时在 `output_gap` 报告。替换字符的 3 字节也计入上限，返回文本总字节数（stdout+stderr）不超过 `max_output_bytes`；stderr 使用 stdout 之外剩余额度，因此 stdout 持续写满时 stderr 会延后到后续页。后台 job 的 `output.jsonl` 按完整记录分页，单条记录超过一页时用记录内偏移分段，超过 8 MiB 扫描上限则明确报错。任务完成时最终报告替换事件流，此时不提供 cursor（`output_mode:"bounded_snapshot"`）；普通任务的 `output` 由 `TaskUpdate` 替换而非追加，因此 cursor 被明确拒绝，只按上限返回快照。
  并行时序：第 18 轮已单独导入并验证 reviewed 的 fixture 修复（parent 独立复跑 `tools::background_shell` 22 passed；本轮模块共 29 passed），因此下面这句描述的是历史状态，不代表第 19 轮的诊断：~~`tools/background_shell.rs` 既有的并行时序失败仍然 OPEN：isolated/serial 才稳定，4 线程运行时可能各次失败不同用例~~。本轮没有为了本轮功能重跑全部并行套件，也没有改写这些 fixture。
- 交互式 stdin 管道与原生 `TaskInput`（第 19 轮）：`cargo test --locked -p deepseek-code-core --lib tools::background_shell::tests`、`cargo test --locked -p deepseek-code-core --lib tools::shell_input`、`cargo test --locked -p deepseek-code-core --lib permissions::manager::policy_tests`，以及真实 CLI 的 `cargo test --locked -p deepseek-code-cli --test task_input`（真实 `dcl` + 本地 mock，SDK stream-json 两轮同一宿主进程）。
  macOS 构建回归：Unix FIFO 创建使用 POSIX `libc::mkfifo`，不可使用 Apple 平台未导出的 `rustix::fs::mkfifoat`/`mknodat`。`background_shell::tests::stdin_fifo_` 三项测试覆盖非 UTF-8 路径、0600 权限与 CLOEXEC、NUL 路径拒绝及创建失败的 errno。macOS CI 必须执行 core/CLI/Desktop 的 `cargo check --locked --all-targets`（分别用 `-p` 选择三个包）并运行这三项 FIFO 测试；Linux 的编译/测试不能替代 macOS 结果，也不证明 macOS 已支持 TaskInput 的 Linux owner identity 校验。
  契约：只有 `Bash interaction:"pipe"` 且 `run_in_background:true` 才创建管道；普通 Bash、前台和 `detach` 仍然使用 null stdin，结果中也不出现 `interaction` 字段。`interaction` 缺省以外的值、缺少后台标志、以及非 POSIX 平台都在创建任务记录之前被拒绝（平台错误不会留下 pending managed shell）。管道是普通 FIFO，不是终端：没有 PTY、resize 或终端信号语义。
  `TaskInput` 是原生内置工具（默认按需加载，不在核心工具集），接受 `task_id` 与 UTF-8 `data` 和/或 `eof`，`timeout` 为等待锁与管道容量的毫秒数（默认 10000，最大 60000），单次最多 64 KiB。返回 `input_mode:"pipe"`、`bytes_written`（操作系统管道已接受的字节，不代表进程已消费）、`bytes_remaining`、`eof`、`stdin_closed`、`already_closed`、`write_complete`、`timed_out`；写超时或写失败（含 broken pipe）都以结构化结果报告已接受字节，绝不自动重试。重复 EOF 幂等；写后 EOF、已退出、已取消、非交互任务和外部会话都会给出明确错误且不改动任务。
  权限：`TaskInput` 不是只读工具，plan 模式直接拒绝，Ask/AcceptEdits 需要确认，Bypass 允许；真实 CLI 的 `cli_task_input_is_refused_in_plan_mode` 验证拒绝结果确实到达模型。所有权以调用方会话的任务库为准，任务 ID 本身不构成授权。
  持久 worker 与逐调用记录（第 20 轮）：真实 worker 用例 `cargo test --locked -p deepseek-code-cli --test task_input_worker`（本地 mock、`worker::ensure_worker`、真实 engine 进程）：第一轮启动不读 stdin 的 shell（脚本写入 shell 与 descendant 的 PID，descendant 只循环、不读 stdin），第二轮由同一个存活 worker 执行 `TaskInput` 并读到 shell 回显；断言 shell 属于 engine 的进程树、descendant 是 shell 的子进程。生命周期按既有策略：shell 运行时 `worker::shutdown` 明确拒绝（"background shell tasks"），随后 `TaskStop` 结束两个精确 PID（PID + 启动时间判定，避免 PID 复用），最后优雅关闭 worker，engine 与 supervisor 均退出。`TaskStop` 只请求取消：owner 先发布终态任务记录、结束 shell future，之后才释放进程级 `ACTIVE` 计数，所以操作系统里的进程消失不等于该计数已释放。两个 worker 用例在显式 `TaskStop` 后都先等终态记录，再在 READY 内只重试上述 documented `background shell tasks` 拒绝；其他错误立即失败，超时诊断包含任务状态和仍存活的 shell 树。该等待/重试只存在于测试 fixture，不改变产品的 `has_active`、权限或关闭策略。`a_second_session_cannot_write_the_workers_shell` 证明他人会话无法写入且不改变任务。
  逐调用记录与钩子（`crates/cli/tests/task_input.rs`）：真实 CLI + 本地 mock 在两轮模型请求中故意复用同一个 provider tool-call id，断言 3 条 `tool_calls` 行（id 为 `<assistant_message>:inv-...`、互不覆盖）、各自 `input`/`output` 与该次调用一致（`bytes_written` 分别等于各自的 UTF-8 字节数，只有 EOF 调用 `stdin_closed:true`，`task_id` 相同）。PreToolUse/PostToolUse 各触发 3 次，钩子的 `tool_use_id` 与持久行中的 `inv-...` occurrence 完全一致（不是 provider id）；PostToolUse 的 `tool_response` 与对应调用的 `bytes_written`/`eof` 对应。`cli_task_input_pre_hook_denial_writes_no_stdin_bytes` 用 PreToolUse deny 证明拒绝发生在写入之前：模型收到 `FIXTURE_DENIED`、无 `bytes_written` 字段、持久行 `is_error=1`、shell 的 `cat` 输出为空。
  拥有者存活（第 21 轮改为内核租约）：`background_shell::start`（后台管理 shell 与前台提升为后台的两条路径）在任务解析出的目录里创建 `owner.lock`，并在拥有者任务整个生命周期内持有排他 `flock`；PID 仍写入 `owner.pid`，仅供诊断和旧记录使用。租约绑定在打开的文件描述上，进程退出或关闭描述符时由内核释放，因此 PID 复用无法冒充它。`owner.lock` 先在与同目录的临时文件上取锁，再用 `hard_link` 发布，链接不会覆盖已有记录——第二个拥有者会得到 "already has a live owner lease"，不能替换存活身份；这也消除了“先发布任务、后取租约”期间被并发探针抢锁的窗口。探针使用共享 `flock`，所以探针之间不会互相误判。真实 shell 不继承该描述符（Linux 下核对 `/proc/<shell pid>/fd`）。
  `OwnerProbe` 区分四种状态：`Held`（持有租约）、`Gone`（租约已释放，或旧记录 PID 确认退出）、`LegacyPid`（只有存活 PID 的旧记录，无法证明是同一进程）、`Unreadable`（缺失/格式错误/无法读取）。`TaskInput` 只接受 `Held`，其余一律拒绝并说明原因（`owner_lost` 结构化结果的 `bytes_written` 保留已接受字节）；`TaskGet` 仍可读，`task_status` 仅对确认已死的拥有者报 `unresponsive`，缺失或格式错误的身份按 heartbeat 维持原有可读行为。
  等待期间的所有者检查：获取 `stdin.lock` 的轮询循环、拿到锁之后、每次管道容量等待迭代、以及写入 EOF 之前都会重新探测。真实用例：`a_full_pipe_write_stops_at_owner_loss`（先填满管道，通过测试闩锁等到真实写入返回 `WouldBlock` 后释放租约，恢复写线程后返回 `bytes_written:0`、无 EOF，读者最终收到的正是先前已接受的字节）；`owner_loss_stops_a_cross_process_write_waiting_for_the_lock`（独立进程调用真实 TaskInput；子进程在真实 `TryLockError::WouldBlock` 分支、也就是通过自身“拥有者仍存活”的初始校验之后写出就绪标记，父进程等到该标记才释放租约，因此第 31 轮观察到的“拥有者在初始校验前已死、提前拒绝”竞态已从构造上排除；该闩锁是 `#[cfg(test)]` 的线程本地钩子，不改变生产行为）。
  生命周期与描述符：`a_cancelled_async_write_stops_and_sends_nothing_more` 曾只在单测通过、在整套 4 线程运行时超时。负面对照 `a_transient_writer_descriptor_is_not_inherited_by_unrelated_children` 证明原因：TaskInput 的临时 FIFO 写端原先没有 `CLOEXEC`，被并发启动的 shell/helper 继承后，即使拥有者关闭自己的写端，shell 也永远收不到 EOF。修复为 `WRONLY|NONBLOCK|CLOEXEC` 后同一对照通过；`prepare_stdin_pipe` 的两端本来就是 `CLOEXEC`。
  engine-only 死亡与 engine+supervisor 全杀是两种情况：`engine_only_death_refuses_input_and_records_the_tree_outcome` 只杀 engine，实测 supervisor 随后退出、根 shell 因 stdin EOF 退出，但**不读 stdin 的 descendant 成为孤儿**；该用例只断言输入契约（`unresponsive`、`TaskInput` 拒绝、零字节），并把进程树结果作为测量输出而非期望行为。
  **明确限制**：租约只覆盖 POSIX `flock`（Windows 走 PID 回退且交互管道不可用，未在本机验证）；检查与最终 `write(2)` 之间仍有竞态窗口，拥有者在这之后退出时已接受的字节无法撤回；旧记录只有 PID 时无法排除 PID 复用，所以交互写入一律拒绝而不是猜测；拥有者被强杀后遗留的进程树（例如不读 stdin 的 descendant）不会自动清理，属既有 owner-death/supervisor 范围，本轮未实现也未声称完成，用例只清理自己记录的精确 PID。
- 内部 Linux PTY 传输基础（第 35 轮，仅内部模块）：`cargo test --locked -p deepseek-code-core --lib tools::pty_transport`。
  契约：新增 `crates/core/src/tools/pty_transport.rs`，只提供传输层；**尚未**注册 `interaction:"pty"`，Bash/TaskInput/TaskGet/TaskStop 与现有 pipe 行为均未改变。`SpawnSpec` 显式给出可执行文件、精确 argv（`OsString`，含非 UTF-8 字节）、cwd、完整环境（先 `CommandBuilder::env_clear()` 再逐项设置，portable-pty 自身仍会补 `SHELL`）和已校验的初始 rows/cols，所有校验都在 `openpty` 之前完成。子进程经 portable-pty 的 `setsid()` + `TIOCSCTTY` 获得真实控制终端（`test -t 0/1/2`、`tty`、`stty size`、`/dev/tty` 均被真实用例覆盖）。master 设为 `O_NONBLOCK`；读写各自使用共享同一 open file description 的复制描述符，都用 `poll` 分片等待并显式处理 `EAGAIN`/`EINTR`，读取把 `EIO` 视为 EOF；后端无法提供 raw fd 时直接失败，不回退到阻塞写。每次读写都有绝对 deadline 和取消标志，取消在等待循环的每个 `poll` 分片（≤50 ms）内重新检查，不会拖到 deadline；共享写闸门用 `try_lock` 分片获取并同样遵守调用方自己的 deadline/取消，因此第二个写入方不会排在一个长期写入方后面超时后才返回，在 gate 等待期间被取消的调用也会在 1 秒内返回 `cancelled`、0 接受字节。超时/取消报告终端实际接受的字节数且不自动重试；单次写入上限 64 KiB、单次读取上限 1 MiB，返回原始字节、不做文本切分。真实用例用“先报 READY、等闸门、再把 stdin 原样回显”的子进程，按实际消费字节数与哨兵位置断言没有迟到写入，而不只是观察一个不读 stdin 的进程没有输出。`resize` 先校验 1..=500 行、1..=1000 列再调用原生 resize，并由真实子进程的 `stty size` 复核。VEOF 仅在实时 termios 仍为 canonical 模式且 VEOF 字符有效时发送（字节取自 termios，本机默认 0x04）；非 canonical（raw）或不可用返回能力错误并保持终端可写；VEOF 不被描述为永久关闭，之后仍可继续写入。
  **终止保证与限制（第 37 轮起为会话范围）**：portable-pty 使直接子进程成为会话首进程（`pid == pgid == sid`）。`stop()` 与 Drop 终止**该会话内**（`/proc/<pid>/stat` 的 session 字段等于 leader PID）的每个存活进程，因此既覆盖 leader 自己的进程组，也覆盖交互式 shell 用 `set -m` 为前台/后台作业建立的其他进程组，以及清理期间新 fork 的同一会话子进程。实现不按进程名匹配、不杀宽泛 PID 范围、不碰其他会话；每个信号都经 pidfd 发出，并在 `pidfd_open` 前后复核创建时钟与 session，因此被回收复用的 PID（session 不同）只会被跳过而不会被误杀。leader 在整个清理完成前保持未回收：只要它未被 `wait`，其 PID 与 session ID 就不会被复用；只有确认会话内已无存活成员后才在剩余预算内回收 leader（此后不再枚举），避免“先回收再按旧 SID 枚举”的复用风险。
  **不确定即失败**：`/proc` 目录项读取失败、`stat` 权限/I/O 失败、stat 记录不可解析，都视为**无法验证**而不是“进程已消失”，清理返回错误、保留 `armed` 和未回收身份、不报告成功；只有 `ENOENT`/`ESRCH` 才按“已退出”处理（由注入式 `ProcSource` 用例分别验证）。超过 5 秒预算时错误列出剩余 PID 与进程组，后续 stop/Drop 仍负责重试。`SessionGuard` 是测试自有、独立于生产清理路径的保险：断言失败时也会清理本夹具的会话，且仅在会话被验证为空后才解除。
  **读/写与停止**：停止请求分两个状态——`stop_requested` 立即让写入返回 `closed`（报告已接受字节、之后不再写入），并让阻塞读停止等待；`cleanup_done` 只在会话验证为空且 leader 已回收后才置位。停止后读走有界非阻塞排空：单次调用最多返回请求页大小、每字节只返回一次，已排队的尾部输出不会因为 stop 而被丢弃；空排空只有在清理已确认或终端真实关闭（读回 0/EIO）时才报 `eof`，清理未完成或失败时报 `cleanup_pending: true` 且 `eof: false`，绝不阻塞等待新输出。真实用例用测试专用的一次性 stop 钩子把“已请求、未清理”的窗口固定住，让仍存活的子进程在该窗口写入尾部，再验证尾部可达且此时不报 EOF。
  **边界**：主动 `setsid`/双重 fork 成为守护进程、或被 service manager 接管的进程已离开该会话，不在保证内，模块不作此承诺。假设本进程是唯一回收该直接子进程的一方（没有外部 reaper，也未把 SIGCHLD 设为 SIG_IGN）；若外部回收导致 `try_wait` 失败，清理保持 armed 由后续 stop/Drop 重试，但不保证成功。未验证：跨进程 IPC 与所有者语义、持久输出/TaskGet、TaskStop 注册、驱动进程死亡、信号与真正的中断投递、Windows ConPTY、macOS 与其他 Unix、真实远端服务。
- 内部托管 PTY 所有者（第 39 轮，仅内部模块）：`cargo test --locked -p deepseek-code-core --lib tools::pty_owner`。
  契约：新增 `crates/core/src/tools/pty_owner.rs`，在传输层之上加入拥有者、私有 IPC 与持久合并输出；**仍未**注册 `interaction:"pty"`，`TaskInput` 明确只接受 `interaction:"pipe"`，Bash/前台执行与 pipe 行为不变，模块只在 Linux 编译。
  **线程模型**：`start` 只创建一个拥有者线程，它自己再创建一个 accept 线程并在退出前 `join` 它；连接由 accept 线程串行处理，最多同时保留 4 个 pending，请求经 `sync_channel(4)` 交给拥有者，没有每连接线程池。accept 线程**不因 pending 满而停止 accept**：控制 `stop` 在满额时仍会被接收、校验并在准入处认领；超过容量的普通请求立即以 `not_executed`/`may_have_been_written:false` 拒绝（不进入 backlog、不占槽位），因此 pending 集合与内存仍有界。`start` 在**调用线程**上先登记 active owner 再创建子进程和拥有者线程；登记 guard 转入拥有者线程，`Drop`/最终清理完成后才递减；启动中途失败走统一回滚（停止会话、写启动失败状态、标记任务 failed）。其后的 `OwnerGuard` 拥有拥有者线程的 `JoinHandle`；`stop_and_join` 直接设置共享 stop/cancel 并写 `cancel` 标记、在预算内等待线程结束、`join` 并返回 `terminal-state.json`，`Drop` 做同样的有界 `join`（SIGKILL 不在保证内）。没有任何进程级 registry 会被忘记 drain。
  **请求生命周期**：每个请求有 `QUEUED/CLAIMED/CANCELLED_BEFORE_EXECUTION` 状态单元；拥有者在产生任何效果**之前**用 `compare_exchange(QUEUED, CLAIMED)` 认领，accept 线程只有在自己的 `compare_exchange(QUEUED, CANCELLED)` 成功时才可回复 `not_executed`/`may_have_been_written:false`。`stop` 不是排队的普通命令：accept 线程先校验信封/所有权并计算绝对 deadline，仅当 `now < deadline` 时以 `compare_exchange(QUEUED, CLAIMED)` **在准入处认领**，随后立即设置 owner stop/cancel 与各 pending 请求的取消位并回复 `cancellation_requested:true, confirmed:false`；因此过期、未认证或未认领的 stop 没有任何副作用，并发的多个 stop 幂等且不存在会把对方取消“清掉”的路径。进行中的长写会观察 guard 停止、跨进程 `TaskStop` 标记、已认证的端点 stop 和对端断连；被认领的写只报告真实已接受前缀与剩余字节，绝不把超时或取消说成“未写入”。已认领或已完成的请求在 `EXECUTION_GRACE` 内等真实回复，超时回 `outcome:"unknown"`、`may_have_been_written:true`。客户端只用一个绝对 deadline，并在发送帧前把**剩余预算**作为 `timeout_ms` 发出，因此连接与组帧都计入调用方窗口；`resize`/`stop` 同样携带该预算，服务端不会回退到默认值。**注意区分**：端点 socket 路径被删除（path loss）不是对端断连；拥有者持有已绑定 inode，观察不到路径删除，所以路径丢失本身不取消进行中的写，取消仍须走 guard/`cancel` 标记等控制路径，且已接受字节与清理结果照常报告。
  **端点与认证**：socket 位于 `$XDG_RUNTIME_DIR/dcl-pty-<uid>/pty-<generation>.sock`（否则临时目录），目录 0700、socket 0600、`terminal.json` 0600；记录 `generation`、`endpoint`、`socket_dev`、`socket_ino`、`token`、`owner_pid`。客户端先按会话/租约/心跳校验，再 lstat（非符号链接）、要求 socket 类型、本人 uid、0600、父目录 0700 且 dev/ino 与记录一致；**连接之后**再用 `SO_PEERCRED` 校验对方 uid 与 `owner_pid`，并重新 stat 路径复核 dev/ino，所以“检查与连接之间被替换”也会被拒绝。服务端对每个请求独立校验 `version`、常量时间 token、`generation`、`task_id`、`session`，不依赖调用方自检。连接使用非阻塞 `connect(2)` + `poll` + `SO_ERROR`（`EAGAIN` 表示 backlog 已满则重试），有绝对 deadline 与取消标志，**不创建连接线程**；独立子进程用例证明 backlog 饱和时反复超时不会增长线程或描述符。
  **输出与终止**：拥有者持续把 PTY 合并输出原样追加到 `stdout.log`，`stderr.log` 留空并在任务元数据标记 `merged_output:true`（不声称分离流）；UTF-8 边界与分页由既有 `output_page` 处理，`TaskGet` 光标/代际/去重尾部规则不变，查询超时不会影响拥有者或子进程。`TaskStop` 写 `cancel` 标记、IPC `stop` 先回 `cancellation_requested:true, confirmed:false`；只有 `transport.stop()` 完成会话清理、最终排空结束后才写 `terminal-state.json` 并更新任务记录一次。**四类事实分开记录**：子进程清理结果、输出捕获（写与 flush 错误、最终排空是否完整）、`terminal-state.json` 写入、`TaskUpdate` 持久化。任一输出失败只算输出失败（`cleanup_complete` 仍可为 true，状态 `failed`），不冒充清理失败；`terminal-state.json` 或 `TaskUpdate` 任一路径失败时 `OwnerGuard::stop_and_join` 返回错误（两者都失败时错误同时指明），保存的元数据在可能时刷新为 `task_record_updated:false` 与错误信息，绝不以 `Ok`/`completed` 加标志位的样子返回成功。最终排空用真实排队字节验证（暂停拥有者、让子进程填满输出队列、再清理并核对日志增长与分页仅出现一次），注入的“尾部”仅用于故障注入用例，不作为“真实读取排队字节”的证据。清理失败时任务状态一律为 `failed`（绝不 `cancelled`/`completed`），并记录 `cleanup_complete:false`、`final_drain_complete`、`cleanup_error`；重复 stop 幂等。写入/刷新完成只表示文件系统接受了字节，**未做 fsync**，不作持久性承诺。
  **明确未完成**：`request_id` 只是关联 id，没有持久去重记录，丢回复一律 `outcome:"unknown"` 且不自动重试；拥有者被 SIGKILL 时不执行 `Drop`。Linux 上由在子进程之前独立 exec 的 supervisor 通过启动握手取得已发布的 session 身份，owner 退出后按 pidfd + start tick 验证并清理该 session 的成员（含 job-control 子进程组）。`tools::pty_owner::tests::owner_sigkill_cleans_the_whole_terminal_session` 用忽略 HUP 的受控夹具使只有 supervisor 能结束成员，独立按 PID+start tick 验证 leader、后台作业和 sub shell 均已退出、无关 sentinel 仍存活、延迟写入没有发生；`supervisor-state.json` 分开记录 `owner_lost:true`、`os_cleanup_complete`、`output_finalized:false`、`task_record_updated:false`；重开任务时 `TaskGet` 依该经 pidfd 验证的 owner 死亡记录**幂等 reconcile** 持久任务为 `failed`（`owner_loss.reason:"owner_lost"`），并分开返回 `os_cleanup_complete`、`output_finalized`（恒为 false，清理进程不等于日志读到 EOF）、`session_published`、`persistence_complete`、`task_record_updated`，绝不写入 `success:true`/`exit_code:0`；已有 `completed`/`cancelled`/`failed` 终态不会被较旧的监督记录覆盖，重复查询不改写记录，输入不重放。`cleaned_pids` 只记录实际接受信号的成员，自行先退出的成员不强制列入，但每个被声明的 PID 必须存在于清理前观测到的 session 中且该实例已退出。owner 在 gate spawn 之后、session 发布之前的 SIGKILL 窗口由 `tools::pty_owner::tests::owner_sigkill_before_publication_never_runs_the_user_program` 覆盖：测试构建在真实 gate 子进程创建之后、`accept_specification`/`session.json` 之前进入受控屏障，记录 owner 与 gate 的 PID+start tick 后阻塞；父测试确认屏障到达、用户命令标记与后代标记从未出现后，只 SIGKILL 确切的 owner。supervisor 如实记录未发布 session（`session_published:false`、`os_cleanup_complete:false`、`status:"failed"`、无 `success`），gate 实例按 PID+start tick 证明退出，无关 sentinel 存活；`TaskGet` 依据同一条 owner 死亡记录把持久任务从 `pending` reconcile 为 `failed`（`session_published:false`），不再永远停留 `pending`，且没有 `terminal-state.json`。该屏障只在 `cfg(test)` 编译，生产构建和模型输入都不可达。正常 owner 结束时监督记录反映 `terminal-state.json` 的真实终态（`tools::pty_supervisor::tests::a_normal_finish_reflects_the_owner_terminal_status`；`tools::pty_owner::tests::a_normal_owner_finish_is_not_recorded_as_a_supervisor_failure` 用自行退出的子进程验证 `completed` 不被写成 `failed`）；缺失或损坏的 `terminal-state.json` 记录为显式 `failed` 加错误（`a_missing_or_damaged_terminal_state_is_never_success`），缺失的监督记录不改变历史表现、损坏的监督记录使查询有界报错且不改写任务（`a_missing_or_damaged_supervisor_record_never_fabricates_a_terminal_state`）。gate 的同 uid 认证与 stale socket 清理仍未验证/未完成；Windows ConPTY、macOS、跨进程 owner 语义扩展、持久 TaskGet 集成与 TaskStop 注册均未完成。
- 内置 `GitHub` 薄封装（第 31 轮）：`cargo test --locked -p deepseek-code-core --lib tools::github`、`cargo test --locked -p deepseek-code-core --lib permissions::manager`、`cargo test --locked -p deepseek-code-cli --test github_tool`。
  契约：单个按需加载的内置工具 `GitHub`（不在核心工具集，只能由 ToolLoad 暴露），操作仅有 `repo_view`、`issue_list/view/create/comment`、`pr_list/view/create/comment/merge`；未知操作在启动任何进程之前拒绝。所有调用以参数向量交给已安装的 `gh`，从不拼接 shell 命令；schema 不暴露 token、host、URL、`confirm` 或任意命令字段，`additionalProperties:false`。`repo` 只接受 `owner/name`（字母数字与 `.`、`-`、`_`，且不以 `.`/`-` 开头），省略时由当前目录解析。
  读操作返回规范化 JSON（蛇形字段；REST 与 `--json` 两种命名都归一），列表用 `gh api --include` 并输出 `page`、`per_page`、`count`、`items`、`has_more`、`next_page`（来自 `Link: rel="next"`），`issue_list` 会剔除 REST 混合返回的 PR 并计入 `filtered_pull_requests`。`labels` 过滤器只属于 `issue_list`（`labels=help%20wanted,bug` 编码保持不变）；`GET /repos/{owner}/{repo}/pulls` 本身没有 `labels` 参数，所以 `pr_list` 带非空 `labels` 会在启动 `gh` 之前被拒绝（`pr_list_refuses_a_label_filter_without_running_gh`，假 `gh` 调用计数为 0；纯校验函数由平台无关的 `the_label_filter_belongs_to_the_issue_list_only` 覆盖），不会静默返回未过滤的 PR。`issue_create`/`pr_create` 的 `--label` 行为不变。原始响应有 1 MiB 硬上限：超过时返回 `error_kind:"output_limit"`，绝不把截断缓冲当作 JSON 解析；完整页面内再按字段裁剪正文（8 KiB，`body_truncated`），因此大于工具展示上限（100 KiB）的合法页面仍能完整解析。其他错误分别报 `gh_missing`、`auth_required`（gh 退出码 4）、`cancelled`（2）、`invalid_json`、`empty_response`、`timeout`，stderr 有上限并去掉 `gho_`/`ghp_`/`github_pat_` 等令牌。
  写操作把正文以 UTF-8 临时文件经 stdin 传入（`--body-file -`），从不进入参数；返回 `created`/`commented`/`url`/`number` 或合并结果。写操作绝不自动重试：超时或疑似传输失败返回 `outcome:"unknown"` 并要求先核对 GitHub；`create` 成功而随后的详情读取失败只记为 `detail_error`，仍报 `created:true`。`pr_merge` 必须给出 `merge_method`（`merge|squash|rebase`），可选 `match_head_commit` 头部守卫，永不生成 `--admin`/`--auto`/`--delete-branch`；gh 退出 0 只代表请求被接受，只有后续读取显示已合并才报 `outcome:"merged"`，否则报 `"accepted"` 并说明可能排队。
  权限：同一工具按操作分类，读操作在 Plan 模式允许、写操作被拒绝；Ask/AcceptEdits 需确认，Bypass 允许；`--disallowed-tools GitHub` 等 deny 规则仍然生效，且"已加载"不构成授权。真实 CLI 用例 `crates/cli/tests/github_tool.rs` 用 PATH 上的假 `gh` 验证：ToolLoad 前请求中没有该工具、加载后 schema 正确、一次读经由正常执行器返回规范化 JSON、Pre/PostToolUse 各触发一次且 `tool_use_id` 与持久的 `tool_calls` 行一致（相互独立的 occurrence id，不是模型给的 id）、Plan 模式与 deny 规则下写操作被拒绝且假 `gh` 从未被启动。
  **明确限制**：全部验证使用本地假 `gh`，未对真实 GitHub 执行任何创建/评论/合并；Windows 与 macOS 上的 `gh` 探测和 CLI 用例未运行（假 `gh` fixture 是 Unix shell 脚本，CLI 用例整体 `#![cfg(unix)]`）；取消/超时只证明自有进程被终止，不证明远端请求是否已被接受，因此写操作一律报 `unknown` 而不是推测结果。
- `tools/mod.rs` 的 shell 启动、`services/tool_hooks.rs`、`mcp/mod.rs` 的 stdio 启动：`worker_environment`，并按 E4 复核后台与分离命令
- `tools/monitor.rs`、`tools/monitor_completion.rs`、`tools/mod.rs` 的 Monitor 生命周期、终态写入重试、schema 与分发：I7（含本地脚本和真实终端中的模型通知）
- `tools/background_shell.rs` 的会话停止扫描、`tools/tasks.rs` 的 Monitor 字段保护与 `native_engine/delegation_task_cleanup_tests.rs`：I7 的子代理清理用例和“子代理结束时停止其后台 shell 任务”。
- `tools/external_agents.rs`：P
- `crates/cli/src/signals.rs`、`crates/cli/src/main.rs` 的信号处理：Q，以及 B（运行中排队与撤回）、E3（`Esc` 中断当前一轮）
- `crates/cli/src/session.rs`、`worker_frontend.rs`、`sdk.rs`、`compaction.rs` 的取消分支：Q，以及 L 的取消场景和 SDK 测试
- `tools/apply_patch*.rs`、`tools/mod.rs` 的 ApplyPatch 注册与分发、`services/checkpoints.rs` 的补丁记录、`native_engine/model_tool_output.rs`：ApplyPatch 一节的三条命令，并在真实终端确认多文件补丁的改动摘要和 `unrestored` 提示
- `native_engine/{command_output_summary,bash_test_summary,model_tool_output}.rs`、`tools/{bash_process,background_shell}.rs` 的捕获与分页、`config/command_output.rs` 和 CLI 的摘要设置：执行下面“长命令输出摘要”的模型请求、设置和捕获回归。
- `runtime.rs`、`db/message_repo.rs`（发给模型的历史）：D2、E5，以及 SDK 测试
- `db/mod.rs`（`write_tx`）：`cargo test --locked -p deepseek-code-core --lib db::write_tx`（重试、非争用不重试、超时报 label、失败不留半行）与 `cargo test --locked -p deepseek-code-core --test session_concurrency`。真实争用复验：用 `VACUUM INTO` 复制数据库，`DEEPSEEK_CODE_DATA_DIR` 指向副本，另一个进程 `BEGIN IMMEDIATE` 占锁 8 秒，期间跑一轮带工具调用的请求，确认回合完成、日志有重试告警、工具行与快照完整；再占锁 40 秒，确认回合以带 label 的错误结束（不是裸 `database is locked`），且没有半成品工具行。
- 启动争用：在已初始化的 `global.db` 上持有写锁，覆盖模型与提供商切换、标题、项目目录回写、Git 工作树绑定和空闲执行租约。运行 `startup_contention_tests`、`git_worktree_binding_waits_for_writer_after_init`。运行 `cargo test --locked -p deepseek-code-core --lib projectless_code_resume_resolves_during_another_directory_write`，确认无项目会话在另一个目录写事务持锁时仍能解析。会话创建和单会话写锁竞争见 `session_concurrency`，多进程首次启动见 `session_db_lifecycle`。
- `db/message_repo.rs` 的请求快照去重（`request_blobs` / `*Ref`）：`cargo test --locked -p deepseek-code-core --lib db::message_repo`（同头只存一份、往返字段一致、旧内联行可读、缺 blob 不伪造、不修改调用方数据）、`cargo test --locked -p deepseek-code-core --test compaction_status_regressions` 与 `cargo test --locked -p deepseek-code-cli --bin dcl status_commands_tests`。历史数据不会自动回填：要缩小现有库需另跑一次性回填脚本并 `VACUUM`，验证方法是回填前后比较 `requests_json` 总字节、`request_blobs` 行数，并抽 3 条消息确认 `get_request_snapshots` 往返一致。
- `services/input_queue.rs`：B、E4，以及“第十一轮：SDK 回合内引导”的全部自动用例
- `sdk.rs`（SDK stream-json 输入与回合控制）：E6、SDK 全套，以及“第十一轮：SDK 回合内引导”
- 权限相关：E2，以及 `added_directory_survives_worker_resume_without_granting_other_sessions`（`bypass` 模式可以读工作区外的文件，其他模式受目录范围限制）
- `permissions/manager.rs`、`tools/path_scope.rs` 的 WebFetch 本地/私有地址：bypass 模式（含 Full Access）直接放行，其他模式仍逐次确认，显式 deny 在非 Full Access 的 bypass 下仍拒绝。自动检查：`cargo test --locked -p deepseek-code-core --lib -- permissions`、`-- tool_approval` 和 `cargo test --locked -p deepseek-code-core --test full_access`。真实终端复验：以 bypass 让 WebFetch 读取 `http://127.0.0.1:<port>/`，确认没有任何权限提示、请求到达本地服务，结果是 `prompt` 的答案而不是原始响应体；切回 default 后同一地址重新出现确认。
- Provider、模型请求：E1、E6、M（OpenCode Go 的预设、URL 纠正、失败提示与会话头）
- `scripts/sync-opencode-go-catalog.mjs`、`src/data/providerCatalog.json`：M，以及 `npm run test:run -- modelCapabilities`
- `interactive_config.rs`：M 的预设、URL 纠正、保存后连接测试和失败提示，以及 N 的刷新命令解析、增删同步、替代模型、默认模型失效、取消不保存和保存失败回滚
- `config.rs`：M 的默认模型读取，以及 N 的偏好文件原子写入、提交失败保持旧文件和文件权限
- `native_engine/model_listing.rs`：M 的网页响应提示，以及 N 的严格列表校验、分页类型校验、原始模型 ID、空列表、失败分页，以及错误中 Key 的多种编码形式和路径凭据都不外泄
- `terminal/editor.rs`、`terminal/input.rs` 的命令补全：`refresh_subcommands`（`/model r`、`/provider r`、`/provider refresh `）
- `native_engine/request_identity.rs`：M 的会话头
- `native_engine/provider_runtime.rs`：M 的目录 baseUrl 推导和四种协议真实对话
- `native_engine/stream_retry.rs`、`native_engine/utf8_stream.rs`、`native_engine/model_http.rs`、`native_engine/request_options.rs`、`native_engine/request_trace.rs`、`performance/`：K（含空闲超时与缓冲总超时的区别），以及 E1
- `project_instructions.rs`、`services/lifecycle_events.rs`：`cargo test --locked -p deepseek-code-core --lib project_instructions`，并确认用户级说明只来自数据目录、项目说明只取用户主目录以下的上级目录
- `native_engine/tool_loop.rs` 的错误与请求快照路径：K、H
- `native_engine/generation.rs`、`services/compaction.rs`、`services/compaction/`：L，以及 E5
- `native_engine/tool_call_ids.rs`（发给模型的请求体）：E1、E6；确认只改写超长调用 ID，未提供 `tool_calls`、`content` 等字段的消息不会被补成 `null`
- `extended_commands.rs` 的复制（`/copy`、`/copy-session-id`、鼠标选择；右击复制后会取消选中并提示结果）：在没有 `xclip`、`wl-copy` 的 tmux 中执行 `/copy-session-id`，显示 `Copied session ID to clipboard`，`tmux show-buffer` 内容为会话 ID；在有 `xclip` 的桌面终端中复制后能粘贴。再跑 E
- `skills/discovery.rs`、`native_engine/skill_tool.rs`、`native_engine/engine_core.rs` 的技能发现与模型列表、`cli/skill_commands.rs`：N
- `services/resource_monitor.rs`、`native_engine/{engine_core,tool_loop}.rs` 的资源告警注入、`crates/cli/src/session.rs` 的监控启动与工作区同步：I43
- 其他范围：至少跑 E

## 长命令输出摘要

```bash
cargo test --locked -p deepseek-code-core --lib -- native_engine::tool_loop::model_output:: capture_counts_original_bytes_and_lines_through_lossy_truncation job_task_get_keeps_raw_sizes_across_the_blocking_page_read tools::output_page::tests:: command_output_opt_out_resolves_each_source_independently command_output_summary_defaults_on_and_can_be_disabled job_task_get_pages_event_output_without_duplicates completed_job_report_is_a_bounded_snapshot_and_refuses_a_cursor running_job_with_previous_assistant_report_pages_current_output task_get_pages_ task_get_without_page_fields_keeps_the_full_snapshot
cargo test --locked -p deepseek-code-cli --test taskget_background --test taskget_backends
cargo check --locked -p deepseek-code-desktop --all-targets
```

- 摘要缺省开启。用 `dcl config set summarize_long_command_output false`（可加全局 `--data-dir`）写入所选数据目录的 `settings.json`，CLI、SDK 和使用同一数据目录的 Desktop 共用此设置。任意来源明确设置 `false` 即关闭，其他位置的 `true` 无法覆盖。JSON 支持顶层或 `tools` 对象内的键，来源为数据目录的 `settings.json`、`config.json`、`customization.json` 及工作区的 `.claude/settings.json`、`.claude/settings.local.json`。TOML 支持 `[tools]`（或顶层）的同名键，读取数据目录的 `config/orchestration.toml` 及工作区和进程目录对应的 `config/orchestration.toml`、`../config/orchestration.toml`。启动时加载的 TOML 变更后可重启；`DEEPSEEK_CODE_SUMMARIZE_LONG_COMMAND_OUTPUT=false` 同样关闭。
- Bash（含 PowerShell）和后台 TaskGet 快照、分页合计 stdout/stderr 的原始字节，达到 12,000 字节才附加摘要。工具截断不单独触发摘要。PTY/ConPTY 控制台任务保留原结果。用 11,999 和 12,000 字节的无效 UTF-8 输入跨过门槛，核对真实模型请求中的快照和分页。完整短输出（包括 44 字节的字面占位符）应与关闭摘要时逐字节一致。
- 用同一后台命令的快照和页验证：`seq -f 'row-%04.0f-diagnostic' 0 599` 的 600 行在快照的 `output`、`result`、`stdout` 中有多份，合并可见区间后没有省略项。重复内容跨多个片段时按源坐标去重；字节按原始宽度，整行数只计完全不可见的行。
- 用 `printf '%012000d' 0; kill -TERM "$$"` 验证 TaskGet 保留信号 15；直接用 `exit 143` 验证正常非零退出。前台失败状态只出现一次。完全可见的流、头尾、已显示的标记行及小于 10 秒的耗时不添加说明。
- `Omitted output:` 最多附五条带行号的隐藏标记行。生成双流、转义文本和无效 UTF-8，测试 9,999/10,000 毫秒边界、跨截断的源行、已有 spill 路径、无省略时逐字节保持原消息，以及 16,000 字节预算。
- 回放跨捕获缺口或 stdout/stderr 的真实 libtest 输出、相互矛盾的 pytest/Jest footer。摘要只引用来源可核对的原文，不计算任何测试计数或推断结果。固定种子生成双流、伪 footer、随机内容、无效 UTF-8 和随机截断边界；逐一校验行号、引文、原始字节数、行数和 16,000 字节消息预算。
- 本地 OpenAI 和 Anthropic mock 覆盖普通调用、后台 TaskGet、恢复/分叉、手动压缩与关闭设置。核对原始事件、SQLite 记录、分页文本和游标；摘要仅出现在模型副本。

### I41. Bash、TaskGet 与 Monitor 的长 base64

持久化每个工具结果时，先对完整结果应用 Read/WebFetch 共用的判定规则，再生成模型文本。普通编码串阈值为 1024 字节，data URI 为 256 字节。单个请求页或快照就是该次调用的完整结果。

```bash
cargo test --locked -p deepseek-code-core --lib -- base64_model_boundary base64_i6_cut base64_review_history base64_shell_thresholds
cargo test --locked -p deepseek-code-core --test native_compaction
cargo test --locked -p deepseek-code-cli --test taskget_background cli_base64_model_boundary_resumes_100_kib_shell_results
```

- 前台 Bash、超时转后台、TaskGet 快照与分页（含超过 1 MiB 的 PTY 日志）、新旧 Monitor 事件均检查普通内容、编码块、行号、原文 SHA-256 和请求重放。前台与后台的 100 KiB CLI 夹具通过本地模型检查新进程恢复。
- 核对 I6 省略统计和保存路径。base64 恢复文件逐字节等于产生本次结果的完整文本，行号指向该文件中的物理行。会话目录无法写入时检查模型收到明确的读取错误，终端和存储仍保留原始结果。
- 对旧的 TaskGet 页执行压缩和 ContextSearch，验证摘要冲突检测使用原始行，检索摘要保留可打开的路径。检查超过 8 MiB 的普通任务仍可分页检索，跨读取边界的非规范编码保持原样。检索路径取自存储字段，覆盖目录名包含 `, line ` 和 Unix 非 UTF-8 字节的情况。
- native_compaction 使用本地网关检查带类型的文本数组、压缩后的第一个请求、两种上下文压力及部分摘要。单元测试检查带钩子的摘要、旧行只读回退和原始结果与模型文本的原子更新，并扫描模型、摘要和检索代码是否直接读取展示字段。

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
4. 让模型执行一次返回 12 行的 `Grep`：折叠结果显示前几行和 `+N lines (click to expand)`；实际点击后能看到最后一行，复制内容为完整文本。
5. 后台任务运行时，`TaskGet(任务名称)` 在 `block:true` 等待结果前出现；恢复历史后仍显示名称。
6. 工具运行期间用独立 SQLite 连接持有写事务 3 秒：前台不弹锁库提示，继续显示工具结果并保留已有建议；持续锁超过 5 秒才提示状态延迟。释放锁后正常结束，过期建议不能获批；任务目录中没有新增 `cancel` 文件。
7. 空标准输入失败后允许保留诊断日志，但不能创建会话数据库或配置；损坏的配置文件不能被覆盖。
8. Windows 短暂独占 `pending-question.json` 后释放，前台仍能读取审批；持续占用时在有限重试后报告具体文件路径。文件已删除则视为审批已处理，其他会话不能读取或回答该审批。
9. Ask 模式弹出普通权限审批时，直接输入 `/mcp state mockweather user-only` 再按 `Enter`：审批保持打开、选项不变，`j`/`k` 不再移动选择，选项下方显示 `Typed ahead: … · Esc keeps it and returns to the choices`。按 `Esc` 把已输入文字移出缓冲（不删除、也不回答），该行改为 `Kept for the input: … · answer the prompt as usual`，随后数字 `1`/`2`、`Enter` 和方向键恢复原有回答方式；回答后这段文字追加到输入框末尾且不自动提交。缓冲非空时按 `Backspace` 清空同样恢复快捷回答。审批因该回合结束等外部原因关闭时，已输入文字（含 `Esc` 移出的部分）按顺序追加到输入框末尾。计划审批、AskUserQuestion 和多选菜单的按键行为保持不变。

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

### 会话分库与 JSONL 导出

1. 运行 `cargo test --locked -p deepseek-code-cli --test session_export`。空数据目录首次启动应创建 `global.db`、`session-locks/` 和 `sessions/`。已发布会话的历史位于 `sessions/<storage_key>/session.db`。核对生命周期目录、准入锁以及 SQLite 的 WAL/SHM 文件。另在启动前放入内容各异的 `deepseek_code.db`、`deepseek_code.db-wal`、`deepseek_code.db-shm`，启动并导出后逐一核对字节与修改时间。Linux 文件事件监视应记录零次打开、改名或删除。
2. 用本地 OpenAI、Anthropic 服务分别生成会话，运行 `dcl sessions export <id>`。标准输出逐行解析 UTF-8 JSON，首行为 `deepseek-code.session` 的版本 1 头。核对归档标志、两种修订号、多模态内容、父子调用 ID、请求及按摘要去重的请求体。运行中的回合先保存 MCP 返回值、再保存工具调用时，第一次导出的 `mcp_output` 应保留 `tool_use_id` 和结果，并带有 `occurrence_pending: true`。调用落盘后再次导出，同一 ID 应能关联到 `tool_call`，标记消失。分支导出保留来源回合 ID，回退后只含保留下来的历史。
3. 保持回合运行，通过管道读到导出头后追加消息。该次导出只含快照内已提交的内容，运行中回合的状态为 `running`。按 owner、字段和索引连接大字段的 Base64 字节，核对长度和 SHA-256 后再解码文本。逐类统计末行之前的记录，并对其原始字节（含每行 LF）计算 SHA-256，与 `end` 核对。截断或校验失败的文件应判作不完整。
4. 运行 `dcl sessions export <id> --output <file>`，核对成功后目标可读、标准输出为空。已有目标时应失败，错误写入标准错误，原文件保持原样。外部附件保留路径引用，导出内容包含原始用户输入与工具结果。

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

会话消息（`SendMessage`/`session_messages`）的引擎侧轮询发生在每个模型/工具边界。收件箱和队列位于 `global.db`，已投递的消息和工具输入写入接收会话的 `session.db`。分别持有目录和会话的写锁，验证轮询能在竞争结束后继续当前回合。

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
- 运行 `cargo test --locked -p deepseek-code-core --lib a_late_acknowledgement_failure_recovers_one_durable_receipt`。在分片投递提交后让目录中的 `session_inbox` 确认更新失败。历史中仍有一条消息，队列状态为 `claimed`，保留 generation，提交时间为空。移除触发器并重放，状态应变为 `context_committed`，历史仍为一条，目录消息数只增加一次，再次轮询返回空。
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
- `cargo test --locked -p deepseek-code-core --test session_db_lifecycle child_upgrade::`：父会话持有执行租约时，恢复 v11 子会话必须完成升级并到达启动步骤；已有共享锁时，直接升级必须返回可重试错误，释放锁后重试成功。两个场景均在有超时和回收机制的子进程中运行。
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
- `cargo test --locked -p deepseek-code-core --lib native_engine::tool_loop::tool_approval`：Coder 计划批准前 Write/Bash/ApplyPatch 在运行时被拒绝，计划文件仍可写；规格/策略不匹配的旧审批不能放行；当前审批放行，ExitPlanMode 切换到 Router 继承的权限模式。ApplyPatch 即使配置为常驻工具也不能出现在 Router 工具列表，直接调用仍须由 Router 编辑门禁拒绝；合入新编辑工具时同时验证展示和执行限制。
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
- 完成记录只写一次（确定性 operation），额外 tick 与重开数据库不会重复事件或重放交付；5B2a 本身不清理工作树；后续 S4a 已接入单 Git 仓库的 `COMPLETED` tick，清理规则与证据见下节。多仓库/非 Git 仍不产生 `CLEANUP_COMPLETED`。非 Git-only 任务即使处于 `FINALIZING` 也不会被该 Git 完成器标记完成，其快照完成规则留待后续增量。
- 多个 Git 仓库按上述顺序交付；Git+非 Git 混合目标仍明确不派发，任务留在 `DELIVERY_PENDING` 并在 tick 报告中给出警告。已观察 A 之后，待交付 B 若检出变脏、目标 ref 漂移或协作锁被释放/被其他任务持有，编排层在启动任何子进程前以点名 B 的原因阻塞：A 的观察与真实 ref 不变、持久不可变计划的 old/new OID 和证据不变、不刷新基线、不重新获取或窃取锁、不产生全局完成事件。
- `cargo test --locked -p deepseek-code-cli --test super_agent_review_loop`：21 项。除原有场景外还有：未检出目标 CAS 成功；带空格路径的 linked checkout 原地快进成功；交付命令失败但成员声称成功时不交付并阻塞；计划已释放且他人持锁时不派发；持久化 `deny Bash` 规则下交付子进程收到真实 `Permission denied: Explicit tool deny rule`；交付前通过 `session_permission_mode` 把 Router 父会话改为 `DontAsk`（Orchestrator 仍以 `bypass` 构造，且不写 Coder 专属 deny）时，恢复的原 Coder 在 Bash 调用上收到真实 `Permission denied: dontAsk mode: ...`，目标、索引与文件逐字节不变、无完成事件、Coder 身份不变；Git 已被移动但宿主尚未观察时重开新 context/store/orchestrator 不重复派发、不在子进程运行时完成、停止后只观察一次；launcher `after-spawn` 崩溃边界（operation 已保留、job 已运行、handle 未写入、receipt 无 job）由新宿主按保留的 job 身份接管，不按旧基线重新校验、不重启子进程；同一崩溃边界下把 operation 的 `task_id` 改成另一个任务（仍写在其正确 key 下）时，新宿主在绑定收据或观察前明确阻塞，零重启、零重复命令、目标保持子进程真实写入后的提交、矛盾记录原样保留；当前 typed 身份的有效 receipt 已保存但从未启动、随后目标被外部移动时，恢复必须走到实时 drift 校验并以 `moved`/`approved plan is stale` 阻塞，零子进程、目标保持外部移动后的提交、无完成事件；已绑定的 receipt 被改指向同一 Coder 的另一个真实 job 时，新宿主在观察前阻塞，收据与 operation 原样保留（不覆盖为原 job 或其他 job）、零重启、零重复命令、目标保持子进程真实写入后的提交、无完成事件。真实集成报告由显式 fixture gate 持有：任务仍为 `INTEGRATING` 时重复 tick 必须复用同一 integration job/operation 且命令次数不变，释放后才允许进入最终审核。每个成功场景验证同一 Coder 会话、真实 Bash 工作目录、目标提交、逐字节内容、交付命令只执行一次、重复 tick 与重开数据库不重复派发。另有两个真实双仓库场景：两个独立 Git 仓库、不同目标分支、含空格路径；每个仓库由真实 `dcl` 子进程执行一次 Bash 交付命令，fixture 拥有的执行日志记录命令实际运行次数，durable `tool_calls` 记录两次 Bash 调用且 invocation ID 不同（provider 在两轮复用同一个 raw `delivery-bash` ID）。成功场景在第二个仓库子进程仍运行时确认没有任务级完成事件，释放后才最终完成；A 成功/B 失败场景保留 A 的观察与提交、点名 B 阻塞，重开宿主后不重放 A、不新增子进程。第三个双仓库场景覆盖“真实写入后、观察前”的宿主重开：B 子进程已执行真实 Git 命令但仍被模型完成门挡住时，丢弃旧 host/context，从同一持久化 DB 重开新 host 并 tick，必须接管同一 typed job/operation、零新增子进程、零 `INTEGRATION_COMPLETED`；释放后新 host 才观察到成功。最终完成后再额外 tick 和再次重开仍只有一个 `INTEGRATION_COMPLETED`，且不产生 `TASK_COMPLETED`/清理；A/B 执行标记各一次、durable invocation 仍为两个不同 ID。第 37 轮另加两个真实完成场景：内容未改变时，同一 Reviewer 被独立 purpose 再次恢复并提交真实的 `FINAL_DELIVERY` Review 与 Harness（工具结果确认两次 `Report recorded`），主机在只有 Review 时必须等待 Harness；内容改变时复用交付前审核、不新增子进程；两者最后都通过事务门禁进入 `COMPLETED`，只产生一个 `TASK_COMPLETED`；S4a 后单仓库的额外 tick 还须验证工作树/任务分支删除、归档 hash 与 OID、恰好一个清理事件，重开 DB 不重复。这些是真实单/双仓库实际交付；`crates/core` 的 `two_repository_deliveries_get_distinct_operations_and_jobs` 使用 fixture shell 子进程，只证明 A/B operation 与 job 互异，不能替代真实 CLI 证据。第 45 轮另加两个真实完成门禁场景（两者均仅 Linux 运行）：最终 `FINAL_DELIVERY` Review 与 Harness 已落库、真实子 Reviewer 仍停在 held 模型响应时，删除该 job 的 `state.json` 后必须阻塞且原因点名该 job，重开宿主仍阻塞、不新建替代 Reviewer、损坏证据保留；对照场景在报告齐全但 job 仍 running 时等待，释放 held 响应、真实子进程正常退出后才恰好产生一次 `TASK_COMPLETED`。就绪由数据库记录、HTTP mock step3 记录和 job 状态屏障证明。负例收尾不复用要求所有 job 成功的通用等待：先恢复被暂停的 supervisor，等它重新发布 `state.json`（避免子进程最后读取控制目录时与删除竞态），再释放响应；该故意损坏的 job 允许真实终态 `failed`/`cancelled`（缺失 state 不是退出证明），其他 job 仍要求 `completed`；子进程与 supervisor 的停止分别按记录的 PID+start 验证，sentinel 必须存活。
- 预取观察者：`cargo test --locked -p deepseek-code-core --lib prefetch::`：2 项。`git status` 预取在独立任务中执行且可能与同一工作树的 `git add/commit` 重叠；观察者必须使用 `GIT_OPTIONAL_LOCKS=0`，否则 Git 会刷新并重写索引（短暂持有 `index.lock`），导致写入方报 `index.lock: File exists`。测试用“内容相同但 mtime 变化”使 stat 缓存失效：对照组证明普通 `git status` 会改写索引，观察者组要求索引字节完全不变且状态仍被缓存；负向对照（去掉环境变量）失败，修复后通过。
- 未验证：失败仓库的受控重试协议、多仓库/非 Git 完成后的清理/工作树/分支归置、Watcher/UI、Git+非 Git 混合交付、非 Git-only 完成；上述“实时父策略”证明引擎按持久化父模式拒绝 Bash，不是操作系统级沙箱拒绝；外部 `git` 与协作锁之间的竞态仍不具原子性；Windows 未运行。


### S4a：已完成单 Git 仓库的持久化安全清理

状态机仅允许已 `COMPLETED` 的任务接收 `CLEANUP_COMPLETED`，状态仍保持 `COMPLETED`，其他终态事件继续拒绝。`COMPLETED` 的后续 tick 单独执行清理，失败只写 `cleanup_status=PENDING` 和持久原因，不改变任务完成状态。多仓库、非 Git 及混合目标明确 PENDING，后续需独立实现；本阶段不代表完整 Super Agent，也不含 Watcher/UI。

- `cargo test --locked -p deepseek-code-cli --test super_agent_child`：首次和同目录恢复后，真实 `dcl --resume <child> --path <another-directory> --print ...` 必须非零退出并明确拒绝。比较全部 SQLite 表的逻辑行、job ID 集合、mock 请求数、原/父/新目录文件及两次 pwd 结果，拒绝路径不得写入或启动新工作。
- `cargo test --locked -p deepseek-code-core --lib super_agent`：清理测试使用真实临时 Git 仓库和既有交付/最终审核 fixture，经真实状态机完成后才执行。覆盖归档在删除之前、base→delivered binary diff、报告与 diff 的 SHA256/OID、目标和源目录不变、重复 tick/重开一次事件；dirty/untracked/ignored 内容保留、外部推进任务分支、错误路径/branch/common/外来工作树、artifact 写失败零删除、Git 分支锁导致工作树已删除但分支删除失败、两处 Git 成功后宿主中断恢复、无删除意图时资产缺失、资产重建、归档损坏、他人锁保留、缺失成员 job/报告、活动或未知子会话、子任务与不支持的清理范围。
- `cargo test --locked -p deepseek-code-cli --test super_agent_review_loop`：真实单仓库完成场景继续验证原 Coder/Reviewer、reviewed/integrated OID、交付一次和完成一次；清理 tick 验证 task worktree 与 agent branch 消失、源 checkout/目标提交保持、归档可读且 hash 匹配，重开仍恰好一次 `CLEANUP_COMPLETED`。未完成和多仓库场景继续保留原 no-cleanup 断言。
- 持久 operation 绑定 task/spec/round/repository、完整 RepoTarget、完成事件和 delivery plan。先保存报告索引与 diff，后保存归档路径/hash 和逐步删除意图。缺失资产只能凭既有意图恢复，已记录删除后重建的资产必须保留。每次恢复先复核当前完成证据、job 状态、Git 注册/身份/交付 OID 和归档，禁止只按路径不存在判断所有权。
- 只使用不带 force 的 `git worktree remove` 和 `git branch -d`；针对源 checkout 与目标分支不同的情况，仅为该命令指定已验证目标作为 upstream，不改持久配置。包括 ignored 文件在内的新增内容一律留待人工处理；`assume-unchanged`/`skip-worktree` 会隐藏修改，因此同样拒绝。归档基线必须是不可变 OID，分支删除前拒绝符号引用，避免重试时触及它指向的目标。目标状态观察不刷新用户索引。只按 delivery plan 中精确 task/lock operation 释放锁。已完成后的清理允许目标分支在交付 OID 上继续前进（交付 OID 必须仍是其祖先），任务分支仍必须精确指向交付 OID；完成门禁仍要求目标精确匹配，不能提前应用此放宽。
- 保守限制：后台 job 的生命周期损坏/未知时，先核对目录 ID、session 和 parent_session；身份完整且明确无关的 job 不阻止清理，相关或身份缺失/冲突/不可读的 job 保持 PENDING 并记录具体原因。无法识别归属的 child-owner 记录仍保守处理。归档超过既有 64 MiB 限制同样保持 PENDING。协作锁不能阻止外部 Git/文件写入；文件系统/Git 与 SQLite 之间不承诺原子性，恢复依靠持久意图与实时复核。Windows/ConPTY、Watcher/UI、多仓库与非 Git 清理未验证/未实现。

相同 cleanup-pending 原因连续 tick 及重开不重复警告，原因变化仍告警；不支持的多仓库/非 Git/混合形态在任何 Git/归档检查前返回。`cleanup_allows_target_to_advance_after_completion`、`cleanup_rejects_target_that_no_longer_contains_delivery`、`cleanup_unknown_jobs_are_filtered_by_readable_identity` 和 `cleanup_pending_warning_is_deduplicated_and_changes_are_reported` 覆盖第 3 轮修正；保留任务分支被推进的拒绝断言。第 3 轮仅跑受影响组，证据见 `super-agent-20261110/codex-round-03-report.md`。

第 2 轮命令、实际匹配数量、退出码与源码 hash 见相邻开发支持目录 `super-agent-20261110/codex-round-02-report.md`；不把历史数量或 core fixture 证据当作真实 CLI/其他平台的通过结果。

### Linux PTY 模型入口（Codex P3）

`cargo test --locked -p deepseek-code-cli --test pty_worker -- --nocapture` 使用本地 OpenAI mock、真实 CLI 持久 worker 与隔离会话库：模型通过 ToolLoad/Bash/TaskInput/TaskGet/TaskStop 运行完整链路，日志输出 `PTY_WORKER`、`PTY_ID`、`PTY_CLEANUP`、`PTY_DB_CALLS`、`PTY_TERMINAL` 作为本次精确身份和清理证据。核心回归保留 `tools::pty_owner::tests` 当前 HEAD 的 38 个独立用例（旧任务草稿写 39；backlog 子进程复跑不重复计数）、`owner_loss` 过滤器实际匹配的 4 项、`tools::pty_supervisor::tests` 3 项；另覆盖 `tools::pty_bash`、`tools::shell_input`、`tools::tasks` 与 `permissions::manager::session_rule_tests`。普通管道运行 `task_input`、`task_input_worker`、`task_input_owner_death`，TaskGet 回归运行 `taskget_backends`、`taskget_background`。按本文必跑命令完成 core/CLI 全套及 Desktop all-targets 编译。

- Linux Bash `interaction:"pty"` 必须同时 `run_in_background:true`；初始 `rows` 1..500（默认 24）、`cols` 1..1000（默认 80）。使用普通 Bash 构造完成的 program/argv/environment/cwd，保留 shell 配置、session environment、权限、hooks、path scope。当前配置了 sandbox、显式 Bash 进程 timeout 或 Monitor 时明确拒绝；不回退到无 sandbox。TaskGet 的等待 timeout 不终止进程。
- OwnerGuard 由运行时持有到真实退出；活跃计数沿用 worker 生命周期。真实 tty 检查覆盖 stdin/stdout/stderr、`/dev/tty`、初始 `stty size`，模型 resize 后必须读到 `40 120`，下一 resume turn 继续输入。活跃时拒绝 worker shutdown；TaskStop 后确认任务 cancelled、清理完成和 PID/start tick 对应的进程退出，再关闭 worker。
- TaskInput 先完整校验参数，再按所属会话中 owner 写入的 task kind 分派。模型不能通过 TaskUpdate 更改 managed task 的 interaction/log_dir，也不能伪造 managed_shell。resize 必须单独调用；与 data/eof 混用、未知字段、非法尺寸/timeout 都在任何写入前拒绝。
- PTY eof 发送当前 canonical VEOF；它不是永久关闭 stdin，有缓冲输入时可能仅交付缓冲。真实用例分别等 PTY 日志出现 `EOF_READY` 和 `VEOF_OBSERVED` 再检查模型取得的快照，并验证 read 返回 EOF 后下一次模型输入仍成功。非 canonical 或禁用 VEOF 的能力错误、部分写入/取消不重放由既有 owner/transport 套件验证。正常 exit 0、非零退出与 TaskStop 分别检查 completed、failed、cancelled，不手工改 task 状态。
- 模型入口验证 plan 拒绝 PTY 启动和 TaskInput、PreToolUse deny 拒绝输入、显式 Bash deny 与 sandbox 拒绝启动；marker 不变。外 session 输入拒绝。TaskGet 分页拼接与完整输出完全相等；SQLite 各调用 occurrence ID 独立，成功输入的 hooks ID 与持久行对应。
- 初始化目录失败不留下 running task 或 child；执行 gate 已启动但目标 exec 失败时，TaskGet 必须给出 failed 与真实清理结果。失败日志保留，不把 Bash 返回 running 当成目标程序已成功 exec。

范围：仅 Linux 接线。Windows/ConPTY、macOS 明确未实现；不以 Linux 编译或内部 owner 测试代替这些平台、Desktop 运行、远程模型或完整长输出压力验收。

### ToolScript 的 Super 身份与公共编辑门禁

ToolScript 保持按需加载。Router 可以加载它来组合允许的工具，但内层 Write、Edit、ApplyPatch 仍须经过直接调用的 Router 门禁；隐藏工具 schema 不能代替执行时拒绝。Bash 沿用直接调用的 Router 权限语义。Coder 的业务写入与 Bash 在当前任务计划批准前拒绝，过期规格审批不能放行；普通会话的持久 plan 模式同样约束内层调用。

- `cargo test --locked -j 2 -p deepseek-code-core --lib toolscript`：包含 `router_can_load_toolscript_but_nested_edits_hit_the_direct_call_gate`，验证默认不加载、加载后 Router 可见、安全 Read 成功、三种内层编辑与直接调用收到相同公共门禁拒绝。
- `cargo test --locked -j 2 -p deepseek-code-cli --test toolscript_super_agent -- --nocapture`：4 项使用真实私有 worker，验证 Router 三种编辑拒绝；Coder 无审批/过期审批拒绝 Write、Edit、ApplyPatch、Bash，当前审批后四者均产生预期文件内容；持久 plan 覆盖 job 的 bypass 快照；损坏 Super 身份在首个调用前持久失败。断言逐调用结果、调用数量及文件副作用，而不只看进程退出或脚本完成。
- engine 和 worker 都调用 `ToolLoopExecutor::with_durable_super_agent`，从数据库加载角色及任务成员关系。读取/解析错误不能降级成普通 Agent。内层仍走 `prepare_tool_call_with` → `execute_tool_call_inner` → `permission_hard_denial_for` / `super_agent_plan_gate`，没有 ToolScript 专用编辑拒绝名单。

### 模型请求压缩（CompactContext）

模型在任务完成后可以请求在下一条用户消息前压缩；当前回合继续用完整历史，请求不会立即执行。历史不足窗口 30% 或刚压缩过时拒绝。

1. `cargo test --locked -p deepseek-code-core --lib compaction_requests`：低于 30% 拒绝且不留请求；达到 30% 保存并只取用一次；刚压缩后只有一条用户消息时拒绝、两条后允许；后一次请求覆盖前一次。
2. `cargo test --locked -p deepseek-code-core --lib tool_loading`：没有上下文用量快照时工具不在内置目录；`Some((50,100))` 时在目录中且默认按需加载；`delegation_depth = 1` 的子代理不提供；`tool_load_loads_compact_context_by_exact_name` 验证 `{"names":["CompactContext"]}` 在下一请求加入工具定义。
3. `cargo test --locked -p deepseek-code-core --lib tool_load_reports_unknown_and_unavailable_names_without_loading_a_partial_list`：大小写错误、关键词、带 `select:` 前缀和逗号拼接的名称均被拒绝。返回具体的未知和不可用名称，混合请求不产生部分加载。
4. `cargo test --locked -p deepseek-code-cli --test session_worker compact_context_is_offered_in_a_worker_terminal_session`：真实会话 Worker（`worker_frontend` 提交、`persistent_worker` 以 `&mut None` 运行回合）中，ToolLoad 后下一请求包含 `CompactContext`。只看 `input.is_some()` 会把真实终端判为无主会话，去掉 Worker 目录判定时该用例失败。
5. `cargo test --locked -p deepseek-code-cli --test standalone a_queued_request_compacts_before_the_next_prompt_with_the_keep_text`：真实 CLI + 本地 mock，排队的请求在下一提示前发送摘要请求并带上 `keep` 文本，之后的历史从新边界开始且请求行已被删除。
6. `cargo test --locked -p deepseek-code-cli --test standalone manual_compact_clears_a_queued_request`：手动 `/compact` 成功后清除排队请求。
7. 真实终端：在历史超过窗口 30% 的会话里，让模型完成任务并请求压缩；下一条提示时确认显示 `Compacting the conversation as the model requested…`，且摘要保留 `keep` 中列出的待办、决定和路径。工具默认按需加载，先经 ToolLoad 加载 `CompactContext`；SDK、`--print` 和子代理中不应出现该工具。回合实际在会话 Worker 里执行（`input` 为 `None`），有主判定必须同时接受 Worker 控制目录；远端/SDK 宿主不得据此启用。

### 压缩后检索当前会话（ContextSearch）

默认通过 `ToolLoad` 加载，从下一次模型请求起可调用。工具目录在新会话中也可供发现，续接说明只在应用了持久压缩边界后出现。搜索只读当前会话已保存的用户及助手文本和工具结果。允许使用该工具的子代理和队友读取各自的会话。可用 `role`、`tool_name`、`range`（`all`、`before`、`after`）和本会话的 `boundary_id` 过滤。省略边界时，第一次请求选择最近一次压缩边界并随游标保留。结果按时间线从新到旧排列，提供消息 ID、已记录的回合 ID、时间戳以及工具调用 ID。

1. `cargo test --locked -p deepseek-code-core --lib context_search -- --nocapture` 验证两次压缩之间的过滤、父会话与实时 Agent、TeamSpawn 队友及续接的隔离、越过行数与字节预算、跨文本窗口的 UTF-8 匹配、结构化结果投影、以及长匹配与结果上限。过滤器也覆盖无存储、关闭工具、未加载和权限拒绝。
2. `cargo test --locked -p deepseek-code-cli --test standalone context_search_recovers` 使用真实 `dcl --print` 与 SDK stream-json 分别连接本地 mock。先通过 Read 保存唯一值，再用 `sessions compact` 提交省略该值的摘要，最后重启并恢复。核对两次搜索前的模型请求没有该值，ToolLoad 加载后才发出 ContextSearch 的 schema，必需的值来自真实搜索结果，且消息 ID、回合 ID、时间戳和工具调用 ID 与 SQLite 记录一致。
3. `cargo test --locked -p deepseek-code-core --test native_compaction context_search_hint_requires_persisted_compaction_without_host_provenance` 检查 Desktop 形状的请求。伪造的边界元数据不会带来续接提示，省略元数据但成功应用已保存边界的请求会收到提示。合并回归见 H、L 以及“模型请求压缩（CompactContext）”。Desktop 检查使用 `cargo check --locked -p deepseek-code-desktop --all-targets`。实际界面场景见 `desktop-regression-tests.md` 的 D3。
4. `cargo test --locked -p deepseek-code-core --lib context_search_cold_reads -- --nocapture` 在冷 SQLite 连接测量跳过四条 32 MiB 记录、以及从深游标继续检索时的读取量。
5. `cargo test --locked -p deepseek-code-cli --test standalone context_search_resume_preserves_legacy_ties` 由真实 CLI 升级已有数据库。对相同排序值，按插入行号判定摘要前后的记录，检查发给模型的历史、两次搜索结果和消息引用。

每页最多检查 25,000 个消息和工具元数据行。24 MiB 的读取预算包含文本、索引和元数据页，每个文本窗口最多 8 MiB。大文本按可独立读取的 64 KiB 分块存储，小型原始记录可直接读取。收到 `next_cursor` 时，用原查询和过滤条件继续，即使本页没有命中也应如此。普通文本可以继续读同一来源的后续内容。需要完整投影的过大或无效结构化结果计入 `skipped_structured_sources`。最多返回 10 条结果及 12 KiB 的 JSON。`has_more` 表示本页还有已发现但未返回的命中，`scan_truncated` 表示扫描因预算停止或有结构化来源被跳过。`excerpt_truncated` 表示摘录省略了来源的一部分，`match_truncated` 表示匹配本身超过 384 字节。

## Super Agent Watcher 核心与安全续接（阶段 6 第一部分）

改动 `super_agent/watcher.rs`、恢复配置、Launcher 续接或 CLI `super_agent_tick` 时：

- `cargo test --locked -j 2 -p deepseek-code-core --lib super_agent::`：包括工具超时保护、指数退避上限和恢复配置边界；继续运行现有状态机、交付身份和清理门禁断言。
- `cargo test --locked -j 2 -p deepseek-code-cli --test super_agent_watcher`：Linux 上真实 detached `dcl` 子进程、本地 SSE mock、临时 Git。覆盖已提交后 kill 只续接一次且 HEAD/提交数不变、审批等待及暂停不恢复、双宿主在 reservation 屏障实际竞争、重试上限与一次 Router 通知、存活旧实例先 TaskStop 后续接、持久退避/时间预算、spawn 后宿主中断重开接管、Reviewer 同会话同工作目录续接、外部 SessionClaim 占用时拒绝新实例。测试仅按自身 Job 的 PID 与进程启动标识发信号。
- 回归 `cargo test --locked -j 2 -p deepseek-code-cli --test super_agent_child --test super_agent_router --test super_agent_review_loop --test agent_fork`；`cargo check --locked -j 2 -p deepseek-code-core -p deepseek-code-cli --all-targets`。

Watcher 只执行一次判定，调度由宿主负责；CLI 现有 tick 调用它，无新增常驻循环或界面。缺失/无法确认 Job 身份或副作用结果时保守 BLOCKED；审批/用户/依赖等待不自动续接。未知工具起始时间使用持久首次观察时间计算超时，没有 timeout 的活动工具持续受保护。恢复预算为自首次中断起的墙钟时间，跨宿主重启保留；不声称实现 token/费用预算。非 Git 缺少事后 hash 基线、未完成副作用工具、Git 合并/变基中间态，以及未由现有 typed delivery 路径证实完成的交付，均要求人工核对，不盲目重放。Windows/macOS 的真实 kill/双宿主场景需对应平台另验；上述 Linux 集成不替代 Desktop/Watcher 常驻调度验证。

### Windows ConPTY（Codex 03；Windows 结果以平台 CI 为准）

Windows 测试编译回归（Codex 04）：core 的 `--lib <filter>` 仍会编译整个 lib 测试单元，不能只检查过滤器命中的函数。Unix shell supervisor 夹具的实际执行测试应为 `cfg(unix)`；纯持久状态 adoption 测试仍跨平台运行。依赖 `/proc/self/fd`、inode 和 `dup/fcntl` 的 owner 辅助函数及其类型应与调用测试一样限制为 Linux。ConPTY 测试中 `std::process::Child` 同时实现 portable-pty `Child` 和标准库 `AsRawHandle`，获取原始句柄必须用 `AsRawHandle::as_raw_handle(&child)` 消歧。改动这些测试后，Linux 跑 `tools::background_shell`、`services::background_sessions` 和 core/CLI/Desktop `--all-targets` 检查；Windows CI 跑下面全部过滤器与 CLI 目标，非零测试结果才算通过，Linux 不执行 Windows-only `tools::conpty` 的空过滤器。

`.github/workflows/platform-tests.yml` 在 `ci/**` push 或 workflow_dispatch 运行。测试列表集中在 matrix 的 `core-filters` / `cli-tests`；每条命令保存退出码与实际执行数，0 项失败，日志以 `runtime-tests-<os>` artifact 保存。Windows 运行 `tools::pty_bash`、`tools::conpty`、`tools::shell_input` 和 CLI `conpty_worker`。Linux 保留 `tools::pty`（owner/transport/gate/supervisor/adapter）、`owner_loss`、pipe owner/input 和六个 PTY/pipe/TaskGet CLI 目标；macOS 验证明确拒绝 PTY。Linux 结果不能替代 Windows 编译或运行。

- Windows Bash PTY 复用 portable-pty ConPTY，仍使用正常 Bash 的配置 shell/program/argv/env/cwd；环境移除/覆盖保持 Windows 大小写不敏感。初始 rows/cols、sandbox/timeout/Monitor 拒绝与 Linux 相同；Windows WSL、cmd（raw 命令行引号）和 pipe 保持明确不支持。
- 控制只在原 owner 进程的 canonical root/session/task 注册表中进行；TaskInput 先经过真实任务会话归属检查。持久 worker resume 可继续写，其他进程明确报不支持，不以 PID/心跳或模型 metadata 授权。TaskGet 读取现有合并 stdout 与分页，等待 timeout 不停止任务；TaskStop 使用正常取消标记。
- ConPTY 的内部 gate 在启动用户 shell 前加入 Job Object，关闭 gate 的 job 句柄后才启动实际命令；不允许 breakaway。owner 最后句柄关闭（含强杀）由 Windows 清理任务树。正常退出/停止后查询 Job Object 的 ActiveProcesses=0，再落真实终态。输出独立持续读取，处理 portable-pty 要求的 DSR 光标查询，避免 ClosePseudoConsole 与输出互相阻塞。
- Windows `eof:true` 本轮明确不支持，data+eof 整次拒绝且不写；Ctrl+Z/Ctrl+D 可通过 data 显式发送，由应用解释，不能当通用 EOF。同步匿名管道写由精确线程句柄 CancelSynchronousIo 取消并 join；错误时 `bytes_written_lower_bound:true,outcome:"unknown"` 不承诺不存在未报告的部分写入，不能自动重放。
- `conpty_worker` 使用本地 mock + 真实持久 worker，Windows 检查控制台、回显、resize、跨轮、TaskGet timeout、Job Object 子孙清理、外 session 与同 session 外进程输入拒绝、plan、EOF/非法参数无效果、正常退出、初始化失败、Git Bash 默认 shell、SQLite 独立调用 ID。`CONPTY_ID`/`CONPTY_TREE`/`CONPTY_CLEANUP` 用固定进程句柄记录身份，清理断言先于测试失败兜底清理。
- 退出码 259 的真实 CLI 用例先等待 PowerShell 输出 `EXIT_259_READY`，固定 gate 的进程句柄并确认尚未退出，再通过 TaskInput 释放真实 `exit 259`；随后仍用原 20 秒 TaskGet 上限断言 `failed`、精确退出码 259、清理完成及同一句柄已退出。不能把冷启动尚未完成时的 `running` 当成 STILL_ACTIVE 误判的证据，也不能延长退出等待或放宽终态断言。
- 并发启动（I14）。在 `windows-latest` 多次运行完整的 `cargo test --locked -p deepseek-code-cli --test conpty_worker -- --nocapture`，保持 `RUST_TEST_THREADS=4`。resize/stop、EOF/正常退出、owner 强杀和退出码 259 四项均请求 `shell:pwsh`，检查实际 worker 返回的 `shell_path` 指向 `pwsh.exe`，并在接受就绪标记前读到该进程报告的 PowerShell 主版本至少为 7。三个共享夹具用例应在原 30 秒窗口内通过 TaskGet 收到 `READY`，并在发送输入或结束 owner 前确认 gate、shell 和后代的固定句柄均存活。主 PowerShell 保持连接真实控制台，安静的后代通过 `Process.Start` 创建且不连接该控制台，仍随原 Job Object 清理。检查窗口尺寸、回显、拒绝无效输入、退出码和精确进程清理的原有断言。Linux 可运行同一目标检查编译，预期执行 0 项。
- 本地 Windows 和托管 CI 使用同一先决条件。预检复用产品的 `pwsh` 解析规则。未解析到 `pwsh.exe` 时，这四项测试会在启动 worker 前明确失败，显示解析结果并提示在 `%ProgramFiles%\PowerShell\7` 安装 PowerShell 7 或更新版本。仅添加到 PATH 的安装可能被内置 Windows PowerShell 覆盖。若解析器选中了 PATH 中的 `pwsh.exe`，仍须通过主版本检查。将用例的请求改回 `powershell` 应由 worker 的实际路径断言拒绝。产品继续按原规则回退，手工验证夹具仍可使用 Windows PowerShell 5.1。
- Windows core 另测 Job Object terminate/最后句柄关闭、过期 gate 拒绝、同步写阻塞时超时与取消、DSR 分片、环境/argv/cwd。`job_member_fixture` 是子进程夹具，普通套件调用不启动子进程；子进程重复输出不增加独立用例数量。

平台结果以对应提交的 Actions 日志为准，未运行项必须单列；Desktop 实际 UI、真实 Windows Terminal 仍需人工验证。Windows owner 强杀后的日志尾部/SQLite 终态 reconcile 尚未移植；TaskGet 保留 unresponsive 与已捕获日志，不虚构 completed 或 cleanup_complete。人工步骤见 [Windows ConPTY 人工验证](./windows-conpty-validation.md)，使用同次检出的仓库夹具与本地 mock。

### 会话持久活动时间

`db/schema.rs` 的 activity triggers 与 `conversation_repo` 的时间更新：跑 core 的 `db::schema::activity_tests`、`db::turn_repo`、`session_workflows`，以及完整 core/CLI 回归和三个包 all-targets check。消息内容/思考、多模态内容、工具结果实际变化与回合结束在同一事务刷新 `updated_at`；重复快照、只读查询、worker 心跳、重开 schema 不刷新。SQLite 活动触发器使用毫秒墙钟，回合完成同时保留精确 RFC3339 结束时间。检查事务回滚不留下时间变化、两个连接中较旧候选不覆盖新时间、十分钟注入回合结束后时间不早于最后结果/回合端点。不回填历史会话，不改 Goal proposal contention 提示。

### 完整模型请求记录（默认关闭）

开关顺序：`--record-requests` 强制开启；否则 `DEEPSEEK_CODE_RECORD_REQUESTS=1/true/0/false` 覆盖数据目录 `settings.json` 的 `requestRecording.enabled`；两者均未指定时才使用 `--full-access` / `--yolo` 的自动开启。显式环境变量或配置 `false` 关闭自动开启；无效环境值视为未指定。运行时按会话保存 CLI 策略，子代理通过显式父会话继承，后台 Worker 参数继续携带 CLI 开关；不会改写 settings。

- `cargo test --locked -j 2 -p deepseek-code-core --lib native_engine::request_recording::tests`：三通道本地 HTTP 请求体与原始 SSE 字节相等；非流式原始 JSON；认证头过滤；并发编号；重试 a1/a2；中途取消保留部分字节；记录关闭/路径不可写不影响响应；Unix 目录 0700/文件 0600；14 天最新修改时间与活跃锁排除；会话策略隔离。
- `cargo test --locked -j 2 -p deepseek-code-cli --test request_recording --test deepseek_harness_headers`：真实 CLI 开关组合、普通请求与自动超限压缩、`model_requests` ID 关联、compaction 文件的摘要/边界/请求序号，以及 `dcl requests SESSION` 的纯文本列表、帮助、不存在提示和路径穿越拒绝。缺少记录时查看命令不得创建数据目录。
- `cargo test --locked -j 2 -p deepseek-code-core --test compaction_truncation --test native_compaction --test compaction_overflow --test compaction_status_regressions`：段摘要、合并、截断恢复的记录阶段与顺序；原生 Responses 压缩和运行中的不透明窗口。`a_truncated_merge_splits_at_the_configured_budget_and_reports_its_own_stage` 必须观察到 7 份记录，阶段依次 segment/segment/merge/recovery/recovery/recovery/final，不能只看 compaction 文件存在。
- 同时跑 `native_engine::request_options::tests`、`native_engine::stream_retry::tests`、`native_engine::utf8_stream::tests`、`native_engine::provider_runtime::tests`、`request_failure` / `reconnect_policy`、CLI `sdk`，最后执行本文必跑的 core/CLI 全套和 `cargo check --locked -j 2 -p deepseek-code-core -p deepseek-code-cli -p deepseek-code-desktop --all-targets`。

记录保存在 `<数据目录>/request-records/<会话ID>/NNNN-<请求ID>-aK.*`，请求体及响应不脱敏；元数据 URL 去掉查询、fragment 和 userinfo。头名称包含 auth/cookie/api-key/apikey/api_key/token/secret/credential/password 或以 -key 结尾时移除；任意头值匹配 Bearer/Basic、sk-/sk_、ghp_/github_pat_、AIza、AKIA、eyJ 前缀也移除；所有头值中，长度至少 32、混合字母数字、仅含 token 字符且不是 UUID 的值视为不透明密钥移除。合法 UUID 保留，但敏感头名称和已知密钥前缀仍优先移除。逐块写并 sync_data；开始时元数据即为未完成，收到完整结束标记或正常非流式 EOF 后标记完成；进程突然退出至少保留初始未完成元数据。

需另行验证 Windows 路径、ACL/文件锁/替换行为、ConPTY、Desktop 实际运行、真实供应商与 Bedrock 二进制事件。Linux 本地 mock 和 all-targets 编译不能代替这些证据。

## 文件记忆接入（2026-11-13）

隔离数据目录运行 `cargo test --locked -p deepseek-code-cli --test memory_files`：两个协议的本地模型 mock 必须在实际首个及恢复请求看到 MEMORY.md 索引和中文相关正文；普通 Read/Edit/Write/ApplyPatch 完成版本化维护，其他目录写入不继承免确认；Plan Read 成功而 Edit 拒绝；无归属 SQLite 哨兵不出现在请求，旧条目数量不变。core 跑 `services::memory_files::tests`、`file_memory`、`memory_scope_grant` 和原 `memory::files::tests`。运行 `cargo test --locked -p deepseek-code-core --lib -- memory prompt`。检查首条用户消息的项目说明后紧跟 MEMORY.md，超过 200 行或 25 KiB 时有截断提示。检索只绑定发起本轮的用户输入，最多五个文件。工具结果、任务通知、同伴消息、压缩和 hook 唤醒均不得触发检索。在两个协议下保存首条消息，手工修改磁盘索引后恢复同一会话，核对首条内容逐字一致，新索引提醒出现在本轮，系统消息内没有索引或摘录。

运行 `cargo test --locked -p deepseek-code-core --lib memory_admission_request_matrix`，检查用户消息入库时绑定检索结果、生成消息跳过检索、原生及文本压缩后索引只出现一次，并逐字节比较三种协议在独立进程恢复前后的请求。运行 `cargo test --locked -p deepseek-code-cli --test sdk user_frame_during_a_tool_run_reaches_the_next_model_request`，检查运行中收到的 SDK 用户帧在工具续轮携带已存的附件。


手工终端：`dcl --data-dir <临时数据> --path <临时项目> memory files list`；用 `put <name.md> <source.md> [--version HASH]` 创建/更新、delete 必须提供版本，过时版本失败。`/memory entries` 新建、编辑器保存、取消和删除确认；另一进程改相同文件后旧面板提交应报 Conflict。`/memory legacy` 与 flat memory 命令仅管理旧归档，不能误报为当前项目文件。Home 使用 `--session`，移动会话工作区后旧能力不能继续写；Plan 不创建记忆目录或修复磁盘索引。Windows 和真实终端的未执行项须独立列为未验证。
### 子代理结束时停止其后台 shell 任务

子代理在自己的会话（`delegation_depth > 0`，子会话 UUID）里用 `run_in_background` 启动的 Bash 任务是子会话的 managed shell；子代理结束（完成、失败或取消）时，委派 owner 按 TaskStop 的同一路径停止仍在运行的任务并等待确认：最终状态为 `cancelled` 的任务记入 `Stopped …`，自行结束（`completed`/`failed`）的任务不报告，已发出停止请求但 5 秒内未确认终态的任务记入 `Requested a stop …`，连取消请求都失败的记入 `Could not stop …`；任务库整体读不出来时记入 `Could not check the subagent's background tasks: …`，不会伪装成“没有任务”。报告/确认共用同一 5 秒预算，期间所有任务库与日志读取都在 blocking 线程池上执行，不占用运行时线程；请求阶段在每次 TaskStop 前检查预算与放弃标志，因此超时后仍在运行的旧请求任务不会再开始新的停止：最多完成它已经准入/已开始的那一次 TaskStop，该次操作的结果可能因此不在报告里，但只要请求阶段没有检查完所有任务，结果就带 `Could not check the subagent's background tasks: timed out after 5 seconds before every task was checked.` 这一行（即使请求任务在超时前自行结束并正常返回也一样）；已被恢复的子代理新建的任务不会被这次清理停止。确认只读任务记录的有效状态，不读日志，因此日志不可读不会掩盖已 `cancelled` 的状态。清理 guard 位于子任务自身的 future 内，被取消（owner 被丢弃）时只发请求不等待，不会漏掉随后才发布的任务。摘要追加到成功结果文本，也追加到失败/取消路径（含 transcript 处理与最终文本持久化失败）的 Agent 错误文本。已结束的任务保持原状态。子代理的前台 Bash 到达超时也不再被杀死，而是转入后台，随后由同一清理停止。

1. `cargo test --locked -p deepseek-code-core --lib task_cleanup`：`a_subagent_background_task_is_stopped_when_the_subagent_ends`（mock 子代理启动后台 `sleep`，委派返回时任务已是 cancelled，之后等待其 PID 最终退出——断言的是“委派返回后进程会退出”，不是返回那一刻已消失；结果含停止行）、`a_finished_subagent_task_is_left_alone`（快速后台任务在子代理结束前完成，状态 completed 且无停止行）、`a_failed_subagent_still_reports_the_stopped_tasks`（子代理第二个请求返回 401，父会话错误文本含 `Stopped 1 background task(s)`，任务 cancelled、进程最终退出）、`aborting_the_owner_stops_a_task_created_late`（多线程运行时丢弃 owner future 后，子任务已发布的后台 shell 仍被取消且进程最终退出）、`a_scan_error_is_reported_to_the_parent`（任务库路径不是数据库时摘要含 `Could not check the subagent's background tasks:`）。
2. `cargo test --locked -p deepseek-code-core --lib background_shell`：`stop_running_session_tasks_stops_only_its_own_running_shells`（同一会话一个运行中、一个已完成，另一个会话一个运行中；只请求第一个）、`a_failing_stop_does_not_skip_the_other_tasks`（B 的 cancel 路径被目录占据时 A、C 仍确认停止，B 记入 failed）、`a_task_that_ends_by_itself_is_not_reported_as_stopped`（请求后任务自行 `completed`，不记入 stopped/requested）、`an_unreadable_log_does_not_hide_a_cancelled_status`（`stdout.log` 换成目录后仍确认 cancelled）、`confirmation_respects_one_deadline_with_a_locked_task_database`（另一个连接 `BEGIN EXCLUSIVE` 持有任务库时，current-thread 运行时上的停止在 6 秒内返回且并发 ticker 仍在推进）、`an_abandoned_request_sends_no_further_stops` / `an_expired_deadline_sends_no_further_stops`（放弃标志或已过期预算下请求阶段不发出任何停止）、`a_request_that_stopped_itself_is_reported_as_incomplete`（请求阶段自行发现超时并准时返回时仍带 `timed out after 5 seconds before every task was checked`，已发布的 A 记入 requested、B 记入 failed；完全空结果同样带该行）、`a_timed_out_request_keeps_its_partial_results_and_stops_no_more`（调用方超时后保留已发布的 A/B，释放阻塞的请求任务后它不再启动新的停止）。
3. `cargo test --locked -p deepseek-code-core --lib a_foreground_call_that_reaches_its_timeout_moves_to_the_background`：前台超时仍转入后台；Ctrl+B 转后台用例保持通过。
4. 真实终端：让子代理启动 `sleep 300` 后台命令后结束；父会话结果出现 `Stopped 1 background task(s)`，`ps` 中不再有该 `sleep 300`。

### 本地协调根与默认关闭设置（R1 第 1 轮）

协调根独立于会话数据目录：`--coordination-dir` > `DEEPSEEK_CODE_COORDINATION_DIR` > `home/.deepseek-code/coordination/v1`；相对路径以调用方显式传入的启动目录（`-C` 之前）为基准。解析、读取设置和只读命令不得创建目录或文件；只有 `CoordinationPaths::prepare_private` 才创建协调子树。

- `cargo test --locked -p deepseek-code-core --lib services::local_coordination`：本轮四个用例必须全部匹配并通过。
- `coordination_paths_ignore_data_dir`：两个互不相关的数据目录及其中不同的 `localCoordination` 设置不参与解析；显式参数 > 环境变量 > home；相对覆盖跟随启动目录而不是 `-C`；空覆盖、缺少 home、根以上路径穿越报错；`database()` 固定为 `root/coordination.sqlite3`。
- `coordination_disabled_creates_nothing`：缺失设置和显式 `enabled:false` 都是关闭；缺失文件不创建数据目录；解析不存在的根后目录项数量不变。
- `coordination_settings_validate_section`：允许 UTF-8 BOM 和无关顶层键；JSON 损坏、根非对象、段内类型错误或未知键报错；成功与失败加载前后文件字节完全相同（从不回写）。
- `coordination_paths_are_private`：Unix/Linux 下新建目录 0700、文件 0600（创建后显式设置，不受 umask 影响）；符号链接叶、普通文件叶、过宽权限的既有根目录被拒绝且不被 chmod；私有文件检查属主与普通文件；空格/Unicode 路径可写入并读回。

原生权限断言仅在 Linux/Unix 上运行，不能当作 Windows ACL 证据；非 Linux 的运行期参与能力拒绝属于后续 owner 适配器，本模块的纯路径解析不承担该拒绝。本轮不创建 SQLite、心跳或 CLI 接线。

#### 第 1 轮修复（描述符相对创建与校验）

`prepare_private` 与 `private_file` 改为从 `/` 开始逐组件用 `openat`/`mkdirat`/`statat` 遍历并使用已持有的目录描述符：解析得到的路径只是定位器。创建后通过该描述符 `fchmod`，不按路径名 chmod；既有对象只校验不修复；`AlreadyExists` 只接受安全的对等目录。过滤命令同上；实际应匹配 17 个通过用例与 1 个默认忽略的 `lc_process_helper`（子进程夹具，不算独立验收）。

- `coordination_created_leaf_swap_never_chmods_target`：`AfterMkdir` 把新目录移走并换成指向无关 0755 目录的符号链接；必须报错，目标目录 dev/inode、0755 模式和哨兵字节不变，目标下不新增文件，钩子只触发一次。
- `coordination_parent_replacement_cannot_redirect_creation`：解析后把父路径替换为符号链接必须失败且替换目录为空；在 `BeforeMkdir`（父描述符已持有）替换时，要么在改名后的原目录下创建成功，要么检测到替换后失败，替换目录始终为空；`private_file` 父路径含符号链接时报错且目标目录无状态文件。
- `coordination_concurrent_prepare_accepts_valid_peer`：两个真实子进程（`current_exe` 加 `--exact ... lc_process_helper --ignored --test-threads=1`，仅子进程环境变量携带模式、夹具目录与 nonce）在缺失单组件根上同时 `prepare_private`；两者 umask 0077，在 `BeforeMkdir` 发布 ready-PID 后等待父进程 release。父进程确认两个不同 PID、两份结果标记的 dev/inode 相同且模式 0700，请求名下恰好一个目录。
- `coordination_competing_unsafe_leaf_is_preserved`：`BeforeMkdir` 放入 0755 目录或符号链接，生产 `mkdirat` 收到 EEXIST；两种情况都失败，前者 0755 与 inode 保留，后者符号链接与目标内容/模式保留。
- `coordination_competing_unsafe_intermediate_is_rejected`：根为 `<夹具>/a/b/coord`，`a` 已是 0700、`b` 缺失；在 `b` 的 `BeforeMkdir` 创建 `b` 为 0755 目录，生产 `mkdirat` 收到 EEXIST。`prepare_private` 必须报错，`b` 保持 0755 与同一 inode，`b/coord` 与 `b` 下任何条目都不存在。空 `mkdirat` 之后的 EEXIST 一律按私有后缀校验（`was_missing: true`），因此合法的 0700 对等中间目录仍被接受，`coordination_concurrent_prepare_accepts_valid_peer` 继续通过。
- `coordination_existing_root_requires_exact_0700`：既有 0400、0500、0755、01700 根都失败且模式/inode 不变；0700 成功且不变。
- `coordination_fifo_rejection_preserves_metadata`：0700 父目录中的 0644 FIFO 被 `private_file(false)` 立即拒绝，类型/inode/模式不变（FIFO 无普通文件内容可比）。
- `coordination_file_reopen_preserves_mode_and_bytes`：既有 0644 拒绝且不变；既有 0600 在 `false` 下打开而不截断/改权限，`true` 报已存在且不替换；缺失叶在 `false` 下可创建为 0600 普通文件；符号链接叶报错且目标字节/模式不变。
- `coordination_uid_mismatch_is_rejected`：`check_uid` 正反例；把真实 `Stat` 复制后只改 `st_uid`，目录创建前校验、既有目录校验和描述符文件校验都拒绝；把目录描述符的 `fstat` 交给常规文件校验也拒绝（证明校验基于描述符而非路径）。
- `coordination_private_file_has_cloexec`：新建与既有返回文件都断言 `FD_CLOEXEC`。
- `coordination_restrictive_umask_still_creates_private_state`：子进程把 umask 设为 0777 后创建两级缺失目录与标记文件，恢复 umask 后报告模式；父进程独立断言 0700/0700/0600 与当前 UID。该兜底仅在 Linux 上经 `/proc/self/fd` 的 `chmodat` 完成；其他 Unix 分支直接报错。
- `coordination_data_dir_env_does_not_split_default_root`：两个子进程带不同 `DEEPSEEK_CODE_DATA_DIR`，解析结果都是同一 `home/.deepseek-code/coordination/v1`，不落在数据目录内，且不创建协调根或数据目录项。
- `coordination_non_directory_ancestor_is_rejected`：普通文件作为祖先时解析报错；直接构造的 `prepare_private` 与 `private_file` 也报错，文件字节与 0644 模式不变。
- `coordination_disabled_creates_nothing` 与 `coordination_settings_validate_section` 现在直接比较字面量 `{ enabled:false, registry:true }`，并覆盖仅有无关键的对象和只含 `enabled:true` 的部分段（`registry` 仍为 true）。

本轮证据只来自 Linux 非特权用户。macOS 缺少 `O_PATH`、无 `AT_EMPTY_PATH` 平台对限制性 umask 的兜底、Windows ACL/句柄语义都未运行；非 Unix 构建只保留可编译的 std 回退，不提供原生身份保证。

macOS 权限位宽度只做过源码核对：Apple 目标的 `rustix::fs::Stat` 是 `libc::stat`，其 `st_mode` 为 16 位 `mode_t`，而 Linux 为 `u32`。所有权限掩码、比较与错误信息中的模式值现统一经 `mode_bits(&Stat) -> u32`，`FileType::from_raw_mode` 仍接收原生 `st_mode`；`grep -n "st_mode &" paths.rs` 必须无输出。本机只安装 `x86_64-unknown-linux-gnu`，未安装其他目标或工具链，因此 macOS 构建仍未实际编译，属于源码核对而非通过证据。

### 版本化注册表存储（R1 第 2 轮）

协调注册表使用独立的 `coordination.sqlite3` 与私有 `domain-id` 标记，不进入会话数据库。`open_rw` 在 `fs::OwnedFileLock` 保护的 `schema.lock` 下（最多等待 2 秒）先 create-new 创建并同步标记，再创建数据库，并在一个 `BEGIN IMMEDIATE` 事务中建立 v1 schema 与匹配的 `domain_meta` 行；标记存在但数据库缺失/损坏、数据库存在但无标记、标记与数据库身份不一致、初始化被中断都只报错，不替换或重建任何字节。`open_read` 使用只读连接，不 bootstrap、不迁移、不执行写 PRAGMA，完全不存在时返回 `None`。写入使用 WAL、`synchronous=FULL`、外键与 100 ms busy timeout，并在每个事务内复核版本；心跳/关闭只对 `lifecycle='open'` 且 generation 匹配的行生效，绝不会复活已关闭的行。

- `cargo test --locked -p deepseek-code-core --lib services::local_coordination`：本轮新增六个用例全部通过，旧有用例保持通过。
- `cargo test --locked -p deepseek-code-core --lib fs::file_lock`：锁实现未改动，仍需通过；见上文“文件锁的平台一致性”。
- `local_coordination_registry_roundtrip`：插入包含全部可选字段（含非 UTF-8 分支引用、仓库/工作树哈希、父实例/worker/job 链接）的行，关闭并重开数据库后逐字段比较；重复 instance_id 被拒绝；generation 不匹配的心跳/关闭返回 false 且行不变；正确心跳更新活动与时间，重复活动保留起始时间；关闭后再次关闭或心跳都不会复活。
- `local_coordination_two_data_dirs_share_root`：两个无关数据目录（不同 `localCoordination` 设置）不参与存储；同一 session UUID 在两个命名空间共存且可分别定位；协调根内不出现数据目录内容。
- `local_coordination_two_roots_isolate`：两个根拥有不同 domain ID，左侧插入的行在右侧读取中为零。
- `local_coordination_unsupported_schema_and_corrupt_db`：新版本号、损坏字节、缺失数据库、无标记数据库、标记/数据库不匹配、空数据库的中断初始化都失败并保持字节/文件不变；完全缺失时只读打开返回 `None` 且不创建目录。
- `local_coordination_redaction_and_native_paths`：`PRAGMA table_info` 列集合与允许清单完全一致（没有 prompt/title/argv/环境/命令字段）；JSON 仅以 base64 表示原生字节，不含禁用字段，也不直接包含控制字节；非 UTF-8 路径与分支字节逐字节往返；`NativePath` 的显示保持不透明。
- `local_coordination_concurrent_bootstrap`：两个线程经 barrier 同时初始化，得到同一 domain 与唯一 `domain_meta` 行，版本为 1，先创建的身份不被改写；持有 `schema.lock` 时 `open_rw` 返回 `Busy` 而不是空的成功注册表。

本轮证据仅覆盖 Linux 上的本地临时根（0700/0600 权限校验只在 Unix 生效，非 Unix 只校验普通文件）；WAL 只读打开在只读文件系统或缺少 `-shm` 写权限时的可用性、NFS/网络文件系统、以及并发初始化中的真实崩溃恢复都未验证。

#### 第 2 轮修复（读取不落盘、拒绝不改写）

写入连接在打开后立即设置 `SQLITE_FCNTL_PERSIST_WAL`，最后一次正常关闭后仍保留私有的 `-wal`/`-shm`；读取端用 `READ_ONLY|NOFOLLOW|NO_MUTEX`，不 bootstrap、不迁移、不执行写 PRAGMA，缺失或不安全的 sidecar 直接报错而不是新建。所有打开路径共用一次 `inspect_state`：先看 `-journal`（任何条目都视为不支持的恢复证据），再用有界、仅既有、no-follow 的私有打开读取 `domain-id`（恰好 36 字节规范 UUID）与主文件头部（`SQLite format 3\0`，写/读版本字节均为 2），并要求已初始化域同时具备私有 `schema.lock`、`-wal`、`-shm`。只有真正空根（或仅有本 bootstrap 的锁）才允许创建标记、数据库和锁；已存在的标记/数据库不一致、孤立 sidecar、无标记的数据库都不改写任何字节。过滤命令同第 2 轮；实际应匹配 16 个通过用例与 1 个默认忽略的 `lc_store_repair_helper`（子进程夹具，不算验收）。

- `local_coordination_read_idle_wal_keeps_entries`：写入一行并关闭最后一个写者后，根目录恰好是主文件、标记、锁、`-wal`、`-shm` 且都是本人 0600 普通文件；读取可见该行，读取期间与结束后条目集合、类型、模式、inode 以及主文件/标记/WAL 字节都不变（SHM 可由 SQLite 正常更新）。
- `local_coordination_read_live_wal_keeps_entries_and_sees_commit`：关闭写者后快照主文件，重开写者并关闭其自动 checkpoint，插入新行后主文件字节仍与之前一致；读取端能看到这条仅存在于 WAL 的提交，且条目集合与主文件/WAL/标记字节在打开前后都不变。
- `local_coordination_missing_or_unsafe_wal_pair_is_preserved`：分别删除一个或两个 sidecar，或把 sidecar 换成符号链接、FIFO、0644 普通文件；两种打开都返回 `Corrupt`，不新增条目，主文件/标记与符号链接目标字节、模式保持不变。
- `local_coordination_rejects_hot_rollback_journal_without_recovery`：子进程用 DELETE 模式、`cache_size=2`、`cache_spill=ON` 打开 0600 且 `user_version=2` 的 30×3500 字节载荷库，`BEGIN IMMEDIATE` 更新后直接 `process::exit(0)` 留下热日志；父进程确认日志存在且带签名、主文件已含未提交页，两种打开都返回 `Corrupt`，数据库/标记/日志字节与条目集合（无新增 `schema.lock`）全部不变。
- `local_coordination_rejects_symlink_fifo_and_open_mode_markers`：标记换成指向内容相同的 0600 外部文件的符号链接、0644 普通文件或 FIFO 时两种打开都返回 `Corrupt`；外部目标字节与根目录条目不变。FIFO 用例在带超时的子进程中执行，若实现先打开后校验会因死锁超时而失败而不是挂住套件。
- `local_coordination_orphan_sidecars_are_errors_without_new_lock`：完全缺失的根读取返回 `None` 且不创建目录；空 0700 根与仅有合法 `schema.lock` 的根返回 `None` 并保留条目；只有 WAL、只有 SHM、两者都有或有无关键时两种打开都返回 `Corrupt` 且条目/字节不变；无标记、无锁的数据库返回有界错误且不新增锁。
- `local_coordination_native_path_validation_fences_insert_and_decode`：外来编码、空/超长路径、内嵌 NUL 的插入返回 `InvalidRecord` 且有效行集合不变；合法的非 UTF-8 路径可经 SQL 往返并用 `to_path` 精确还原；直接 SQL 注入坏编码、空路径、非 UUID 实例或负时间戳后 `rows()` 返回 `Corrupt`，每次恢复后重新可用；未知编码的 `to_path` 也被拒绝。
- `local_coordination_sqlite_corruption_maps_to_corrupt`：0600 垃圾主文件配合匹配标记时两种打开都返回 `Corrupt` 且文件布局与字节不变；另外直接构造 `SQLITE_NOTADB`/`SQLITE_CORRUPT`/`SQLITE_BUSY`/`SQLITE_LOCKED` 并断言映射到 `Corrupt`/`Busy`，即使坏头部预检通常先拒绝也能覆盖映射本身。
- `local_coordination_write_rechecks_schema_after_immediate_begin`：对端持有 `BEGIN IMMEDIATE` 预留时 `Store::write` 返回 `Busy` 且闭包不执行；释放后对端把 `user_version` 改为 2，同一已打开句柄再次写入返回 `UnsupportedSchema(2)` 且闭包仍不执行（覆盖 IMMEDIATE 与事务内版本复查）。
- `local_coordination_rows_are_sorted_and_capped_at_101`：逆序插入 110 行、时间戳重复且交错，`rows(usize::MAX)` 与 `rows(100)` 都恰好返回按 `(started_ms, instance_id)` 排序的前 101 行，`rows(1)` 返回前两行（包含截断预读行）。

原生编码证据只在 Linux 上执行：Windows 的 UTF-16LE 偶数长度与零码元检查、macOS 的 `O_PATH`/procfs 兜底、只读文件系统或缺少 `-shm` 写权限时的 WAL 读取、NFS/网络文件系统，以及真实崩溃（而非受控子进程退出）都未验证。

#### 第 2 轮修复二（保持 SQLite WAL 预留锁）

SQLite 管理的主库、`-wal`、`-shm` 与回滚日志不再由本进程用独立描述符打开：独立描述符的关闭会清除该进程在同一 inode 上的 POSIX 字节范围锁，从而释放其他连接持有的 WAL 写预留。这些条目改用 `statat(..., SYMLINK_NOFOLLOW)` 元数据校验（普通文件、本人、0600）；主文件头部通过 `SQLITE_FCNTL_FILE_POINTER` 取得 SQLite 自己的 VFS 文件并用 `xRead` 读取（魔法 `SQLite format 3\0`，物理 WAL 版本字节 18/19 均为 2），随后仍用 SQL 事务确认 `user_version` 与 domain。标记与 `schema.lock` 仍用私有描述符（创建、加锁需要 FD）。非 Unix 构建把 `anyhow_error`/`is_missing` 等 rustix 依赖置于 `cfg(unix)`，Store 入口继续返回 `UnsupportedPlatform`。

- 过滤命令同上；实际应匹配 18 个通过用例与 1 个默认忽略的 `lc_store_repair_helper`（子进程夹具，不算验收）。
- `local_coordination_public_store_open_preserves_peer_process_writer_lock`（仅 Linux）：父进程打开写连接并持有 `BEGIN IMMEDIATE` 预留；子进程观察者只读打开 `coordination.sqlite3-shm`，以 `F_GETLK` 查询字节 120（SQLite 3.45 Unix 的 WAL 写锁字节），记录锁类型与持有者 PID，绝不调用 `F_SETLK`。第二次打开（读用例 `open_read`，写用例 `open_rw`）之后再次观察，必须在**第二个句柄与事务都仍存活**时仍看到 `F_WRLCK` 且持有者为父进程 PID；回滚并关闭后额外观察一次应为 `F_UNLCK`。观察在两次用例中使用各自的临时根，结果的 nonce 必须完全相等，旧结果文件会先被删除。
- 快照规则：涉及 SQLite 管理文件的字节比较一律由没有该库任何连接的子进程（`snapshot-sqlite-bytes` 模式）复制到 `fixture/` 下的独立文件后由父进程读取；父进程只在相关句柄全部关闭后才直接读主库/`-wal` 字节。`local_coordination_read_live_wal_keeps_entries_and_sees_commit` 现用该子进程快照证明“新行只在 WAL 中”（主文件字节不变、WAL 字节变化）以及读取前后条目集合不变。
- `local_coordination_rejects_hot_rollback_journal_without_recovery` 增加带真实 `schema.lock` 的第二阶段：拿到锁后仍以 `Corrupt` 拒绝热日志，数据库/标记/日志字节与包含该锁在内的条目集合完全不变。

平台证据：观察者与锁字节位置只在 Linux 上验证；macOS 的锁语义、Windows 的无 POSIX 锁路径、只读挂载与网络文件系统都未运行。非 Unix 仅做了宿主上的 cfg 类型检查（可选 surrogate），不构成原生编译或运行证据。

#### 第 3 轮：Linux 所有者证明与不可变租约

所有权证明从 `tools/background_shell.rs` 抽到 `services::local_coordination::process_identity`（`LinuxIdentity`、`valid_pid`、`read_boot_id`、`process_start_time`、`pidfd_exited`），Bash/TaskGet 的 `OwnerIdentity` 只是同一实现的别名，锁、租约和超时语义不变。新的 `OwnerLease` 把身份与随机 generation 先写入私有 staging 文件并 `fsync`，取得排他 `OwnedFileLock` 后用不能替换的硬链接发布为 `owners/<instance>.lock`，`fsync` 目录后只删除 staging；租约文件永久保留，复用同一 UUID 即使旧所有者已死也拒绝覆盖。`probe_owner` 以仅既有、有界、no-follow 的私有打开读取租约，要求 version、instance、generation 与 owner 全部等于数据库记录；身份在 `pidfd_open` 前后各校验一次并 poll 退出。精确证明已死的进程返回 `Gone`（即使 fork 子进程仍持有描述符），活进程且租约仍被持有返回 `Alive`，活进程但租约已释放、证据缺失/损坏/不可读一律 `Unknown`。探测不发送任何信号，只短暫取得共享锁且立即释放。

- `cargo test --locked -p deepseek-code-core --lib services::local_coordination`：本轮应匹配 35 个通过用例与 2 个默认忽略的进程夹具。
- `cargo test --locked -p deepseek-code-core --test local_coordination_process -- --nocapture`：6 个通过、1 个默认忽略的 `lc_process_helper`（子进程夹具，不算验收）。
- `local_coordination_owner_killed`：独立所有者发布就绪后，父进程只对夹具 PID 发 `SIGKILL` 并 `wait`，探测从 `Alive` 变为 `Gone`，不依赖超时或心跳。
- `local_coordination_pid_reuse`：夹具记录一个真实存活 PID 但写入错误的 start tick/boot；探测返回 `Gone`，且该存活进程未被信号终止。
- `local_coordination_live_stale_heartbeat_retained`：集成用例确认打开数据库后活锁租约仍为 `Alive`；模块内用例通过 crate 内 `Store::insert` 写入 `heartbeat_ms=1` 的旧行，证明注册表年龄不参与所有者证明（不扩大 Store 变更可见性）。
- `local_coordination_duplicate_descriptor_release`：fork 子进程继承同一打开文件描述；错误 generation 的探测不释放租约，随后 `Alive`；显式释放后即使重复描述符仍打开，锁也被释放（`flock` 立即成功），探测变为 `Unknown`。
- `local_coordination_preexec_descriptor_owner_death`：单线程夹具 fork 一个阻塞在管道 gate 上的子进程保留描述符；杀死原所有者后锁仍被继承描述符持有，但探测按记录身份返回 `Gone`，另一活所有者对照仍为 `Alive`。子进程从不执行父进程的解锁析构。
- `local_coordination_owner_unknown_is_not_gone`：缺失、非法 JSON、generation 不匹配、0644 租约、不可解析 creation tick 全部返回 `Unknown`，不构成回收许可。
- 邻接回归：`--lib fs::file_lock`（锁一致性场景）、`--lib tools::background_shell`、`--lib tools::tasks`、`--lib services::session_messages`、`--lib services::session_worker`，以及 `-p deepseek-code-cli --test taskget_backends --test taskget_background --test session_messages`；委派与前台超时用例必须与抽离前一致。

平台证据：进程身份、`pidfd` 与继承描述符场景只在 Linux 上运行。macOS/其他 Unix 的锁语义、原生 Windows、只读挂载、NFS 网络文件系统未验证；非 Linux 的 `acquire` 返回 `UnsupportedPlatform`、`probe` 返回 `Unknown`，只有类型检查证据。

#### 第 3 轮修复（不可读证据与夹具所有权）

`probe_owner` 现在先验证证据再解释身份：解码 JSON 后只接受当前租约版本与目标实例，随后 `identity_from_record` 调用 `OwnerRecord::validate`（Linux 平台、正 PID、规范 boot/generation UUID、有界 creation token）、`valid_pid` 上界与 creation 十进制规范形式；验证失败一律 `Unknown`，通过后才比较期望记录并查进程。`acquire` 用同一规则在创建私有 staging 文件之前校验本条记录。

进程判定区分“有效观测到的差异”和“无法观测”：同一 boot 内 creation tick 明确不同、或 boot UUID 规范但不同，是 `Gone`；`/proc/<pid>/stat` 的 `ENOENT`/`EACCES` 等不可读证据本身不构成死亡，必须再取 `ESRCH` 或已固定 pidfd 报告退出才算 `Gone`，其余一律 `Unknown`；pidfd 的 `EPERM`/`ENOSYS`/poll 错误都不是死亡证据。只有两次 tick 读取一致（`verified`）时才检查租约锁，活进程 + 已释放租约仍为 `Unknown`。`prove_same_boot` 通过闭包注入观测，测试不修改宿主 procfs 或权限。

进程夹具改为所有权安全：`OwnedChild` 持有 `Child` 并在正常、超时与 unwind 路径都通过该句柄 kill+wait；FIFO keeper 由测试父进程以 close-on-exec 打开，helper 只打开读端再 fork，并在父分支读取 fork 子进程的 start ticks 后发布。`ForkGuard` 在 `pidfd_open` 前后各校验一次已发布 start 与 helper PPID，之后只用 `pidfd_send_signal` 或接管 EOF 结束子进程；不再对数字 PID 发送信号，也没有丢弃 gate 后的 sleep。

- 过滤命令同第 3 轮；本轮应匹配 38 个通过库用例、2 个默认忽略夹具，以及 `local_coordination_process` 的 9 个通过、1 个默认忽略。
- `local_coordination_unreadable_owner_stat_is_unknown`：注入首个或第二次 stat `ENOENT`（配活 pidfd）与 `EACCES`（两个位置），全部 `Unknown`，且从不检查锁；首个 `ENOENT` 分支仍会 pin。
- `local_coordination_missing_owner_stat_requires_pidfd_death`：stat `ENOENT` + pin `ESRCH` 得 `Gone`；缺失读取 + 成功 pin + 退出得 `Gone`，并断言确实轮询了固定描述符。
- `local_coordination_pidfd_errors_do_not_prove_owner_death`：pin 返回 `EPERM`/`ENOSYS`、或在 tick 匹配/缺失后 poll 返回 `EIO`，全部 `Unknown`，锁闭包被调用即 panic。
- `local_coordination_malformed_matching_identity_is_unknown`：先以真实持有租约证明活本进程 `Alive`；随后分别把租约与期望记录改成相同非法 boot、非法 generation、带前导零的 tick（保持 inode 与 0600），全部 `Unknown`；换成另一个规范 boot UUID 则为 `Gone` 且本进程仍在运行。
- `local_coordination_fixture_cleanup_reaps_child_on_unwind`：`catch_unwind` 中故意 panic，守卫在 unwind 时终止并回收 helper；之后 pidfd 可读，且 `waitpid(..., WNOHANG)` 返回 `ECHILD`。
- `local_coordination_reused_uuid_preserves_original_lease`：同一 UUID 在租约被持有时与 owner 被 kill/reap 后都返回 `InvalidRecord`，inode、字节、0600 模式与 owners 目录条目集合均不变，无 staging 残留。
- `local_coordination_duplicate_descriptor_release` / `local_coordination_preexec_descriptor_owner_death` 使用已验证的 fork pidfd：显式释放后与杀死原 owner 后都断言 fork 子进程仍存活，释放 gate 后经 pidfd 观察到退出；重复描述符场景由仍存活的 helper 自己 `waitpid` 回收子进程并写 `reaped` 标记。

平台证据仍是 Linux-only；不可读 stat/pidfd 错误用注入闭包覆盖，不依赖受限挂载或权限变更。

#### 第 4 轮：注册表生命周期与工作区快照

`registry.rs` 把存储与所有者证明变成可用的注册服务：`RegistryGuard::register` 先 `Store::open_rw`（空根上先建 `owners/` 会成为 bootstrap 拒绝的孤立证据），再发布租约，最后写入 open 行；插入失败时丢弃租约但保留不可变证据。心跳线程独占 Store 与租约，每 5 秒把最新活动与心跳合并成一次带 generation CAS 的写入，Close 会等待该线程排空后关闭行并释放租约；`Drop` 只发送停止信号，不做无界等待。`update_workspace` 与心跳、关闭同样以 generation + `lifecycle='open'` 为栅栏，过期 generation 或终态行是空操作。`registry_snapshot` 只读：先用 `WHERE lifecycle='open'` 再排序限行（第 101 行仅作截断预读），因此旧的终态行不会吃掉 100 行窗口；open 但 owner `Gone`/`Unknown` 的行仍然显示。`workspace.rs` 用规范化路径字节加 Linux dev/inode 计算数据域、仓库与工作树哈希（各字段长度前缀），有界只读 Git 探测（2 秒、64 KiB、空 stdin、超时 kill/reap、每次只取一个路径输出）失败时 Metadata 缺失而不是伪造仓库；`escaped_native` 把非可打印字节渲染为 `\xNN`。

- `cargo test --locked -p deepseek-code-core --lib services::local_coordination`：本轮应匹配 44 个通过用例、2 个默认忽略夹具。
- `cargo test --locked -p deepseek-code-core --test local_coordination_process -- --nocapture`：10 个通过、1 个默认忽略的 `lc_process_helper`。
- `local_coordination_guard_heartbeat_and_close`：受控时钟推进一次只产生一个心跳；十次 `set_activity` 合并进下一次写入；`close` 排空后行为 `closed`；随后同 generation 的 `heartbeat`/`close` 都返回 false 且行不复活；再次注册得到新实例与新 generation。
- `local_coordination_live_cap_applies_after_lifecycle_filter`：先放 101 条更早的 closed 行再加 1 条 open 行，快照只显示该 open 行且未截断；再加 105 条 open 行后正好显示 100 条、`truncated` 为真，且第 101 条 open 行（预读行）不出现。
- `local_coordination_grouped_frontend_worker`：两个真实进程以同一 namespace/session 注册 frontend 与 executor（带 worker 链接），快照为同一组、两个不同 owner PID 且都为 `Alive`；同一 session UUID 换一个数据目录后成为第二组；全部关闭后不再出现在快照中。
- `local_coordination_snapshot_is_read_only`：完全不存在的根返回空快照且不创建目录；活但心跳很旧的 owner 是 `Unresponsive` + `Alive`，无租约的 owner 是 `Unknown`，两者都保留；重复读取前后根目录条目与行内容完全不变（不恢复 worker/job）。
- `local_coordination_workspace_paths`：临时 Git 仓库与其 linked worktree 共享 repo ID、worktree ID 不同；独立 clone 的 repo ID 不同；符号链接别名与真实路径同一身份；非 Git 目录 repo/worktree/branch 为 null；detached HEAD 的 branch 为 null。另有 `local_coordination_workspace_escapes_native_bytes` 验证非 UTF-8 与控制字节的往返与转义。
- `local_coordination_clock_jump_is_not_death`：向前/向后墙钟跳变只改变 `Responsiveness`（`ClockUncertain`/`Unresponsive`），owner 仍是 `Alive`，行保持 open，没有清理或接管。
- 平台证据：Hash 的 dev/inode 部分与 Git 场景只在 Linux 运行；macOS/Windows 只做了类型检查，原生运行未验证。Git 元数据测试不做出任何 claims 变更。

#### 第 4 轮修复（共享工作区校验、独占心跳线程与有界 Git 读取）

工作区字段的校验规则集中到 `types::validate_workspace_fields`（绝对原生路径、仓库/工作树各为 64 位小写 hex、引用 1–1024 字节且不含 NUL/LF、观测时间非负），`Registration::validate`、`WorkspaceIdentity::validate`、`Store::update_workspace` 与 `Store::heartbeat_with_workspace` 共用同一实现，因此任何被拒绝的刷新都不会写库，也不会让下一次快照解码失败。Git 生产者把超限或非法引用降级为 null，注册时 `last_git_observed_ms` 取工作区自身的 `observed_ms`。心跳与刷新在同一 `Store::write` 事务里写入（先心跳行、命中 open 行且 generation 一致时才写工作区），刷新不再顶替心跳。

注册改为每次一个独占的 `dcl-registry` 线程：先 `Store::open_rw`，再发布租约、校验工作区、插入 open 行，最后才向调用方确认成功；无 Tokio 运行时也能注册。`Mailbox { activity, refresh, stopping }` 只有一个最新值槽位，`request_workspace_refresh` 只覆盖路径，`update_workspace` 用有界 `try_send`；循环用单调 `next_tick` 推进并跳过错过的刻度。`close` 先停止、等待 worker 完成“关闭行→释放租约”后返回；`Drop` 只发停止信号。

Git 读取改为增量排空：`run_git_command` 在 spawn 前检查截止时间，`collect_git_output` 用 rustix 的 `fcntl_getfl/setfl` 设非阻塞、每次最多读 8 KiB、每轮检查截止时间、64 KiB 上限与 `try_wait`，失败时只 `kill`/`wait` 自己拥有的子进程。`git_path` 只去掉一个命令分隔 LF 并保留内部/结尾原生字节与非 UTF-8 字节。

- 过滤命令同第 4 轮；本轮应匹配 55 个通过库用例、3 个默认忽略夹具（`lc_process_helper`、`lc_store_repair_helper`、`lc_git_fixture_helper`），以及 `local_coordination_process` 的 10 个通过、1 个默认忽略。
- `local_coordination_invalid_workspace_updates_leave_snapshot_unchanged`：分别提交相对路径、非法 repo/worktree ID、空引用、1025 字节引用、含 NUL/LF 引用与负观测时间，全部 `InvalidRecord` 且固定 `now_ms` 快照逐字段不变；随后合法改动可读；过期 generation 的写入是 `false` 空操作。
- `local_coordination_registration_uses_workspace_observation_time`：仓库观测时间 1000、注册时钟 2000 → `last_git_observed_ms==Some(1000)`、`started_ms/heartbeat_ms==2000`；无仓库对照为 `None`。
- `local_coordination_workspace_refresh_coalesces_and_preserves_heartbeat`：A 被门控时提交 B、C，下一次观察到的是 C；A 的刻度提交其工作区 + 心跳 2000 + `Active`；C 返回 `WorkspaceUnavailable` 时丢弃工作区但仍推进到心跳 3000 + `Idle`。
- `local_coordination_close_drains_a_gated_inflight_refresh`：门控观察进行中首次 `poll(close)` 为 `Pending`；放行后 `close().await` 返回即读到 `Closed`、新工作区已提交、owner `Unknown`，同 generation 的迟到心跳返回 `false`。
- `local_coordination_register_without_runtime` / `local_coordination_register_on_stopped_runtime`：无运行时或进入已丢弃运行时句柄时注册仍得到活 guard 与一条 `Alive` open 行，随后在新运行时上 `close` 得到 `Closed` 且租约释放。
- `local_coordination_bare_drop_closes_and_releases_lease`：不调用 `close` 直接丢弃 guard 并关闭其运行时，进程仍存活时受限轮询到 `Closed` + `Unknown`，原租约文件保留。
- `local_coordination_git_drains_pipe_backpressure`（Linux）：4 KiB 管道输出 32 KiB，增量排空取得完整载荷；先等子进程退出再读会死锁。
- `local_coordination_git_deadline_covers_inherited_stdout`（Linux）：直接子进程已成功退出但另一子进程仍持有 stdout 写端时，收集器在短截止时间内返回 `None`，且从未向该子进程发信号。
- `local_coordination_git_rejects_output_over_cap`（Linux）：60 KiB 对照被完整捕获，64 KiB+1 字节返回 `None`。
- `local_coordination_git_paths_preserve_lf_and_non_utf8`（Linux）：目录名含内部 LF、结尾 LF 与非法 UTF-8 字节的真实仓库，其规范化原生工作区字节、repo/worktree ID 与 `refs/heads/main` 均精确保留；符号链接别名得到同一身份。
- 分组进程夹具改为逐个等待 ready、快照后再逐个释放并回收，避免无关的启动/关闭争用。

平台证据：Linux 之外的注册（`local_coordination_unsupported_registration_creates_nothing`）与原生非 Linux 运行未验证；Git 管道的截止时间/上限证据仅来自 Linux 合成夹具，不涉及真实仓库构建。

#### 第 4 轮修复二（stdout EOF 后仍等待并回收自有子进程）

`run_git_command` spawn 后统一走 `finish_git_child`：即使 `child.stdout.take()` 为空、fcntl 设置失败、读取/状态错误、非零退出、超限或超时，也都会在返回 `None` 前回收自己拥有的直接子进程。`collect_git_output` 把“stdout EOF”和“直接子进程已成功退出”当作两个独立观测：只有两者都成立且未过截止时间才返回字节；只有 EOF 时不再重复读取，而是以最多 5 ms 的间隔继续轮询状态；子进程已退出但仍可能排空剩余输出；不进行阻塞式 `wait`。`reap_owned` 先 `try_wait`，已有缓存状态就不再发信号，随后只 `wait` 自己的子进程，绝不按数字 PID 发信号、不杀进程组、不遍历后代。

测试夹具新增 `GitFixtureChild` 守卫（Drop 中 `try_wait` / `kill` / `wait`），`spawn_git_fixture` 返回该守卫；keeper 作为 stdin 门控时总是先关闭门控再断言收集结果、截止时间或退出状态，超时路径先放行 keeper 再 join。`lc_git_fixture_helper` 新增 `eof:<code>` 模式：写入固定 `coord-eof-payload` 并 flush，用 `libc::close(STDOUT_FILENO)` 关闭自己的 stdout，等待私有 stdin 上的 EOF 后以 `code` 退出。

- 过滤命令同第 4 轮；本轮应匹配 58 个通过库用例、3 个默认忽略夹具。
- `local_coordination_git_eof_before_exit_returns_payload_after_reap`（Linux）：门控持有期间观察到 EOF 通知，收集结果仍为 pending；放行后退出码 0，收集结果以 `coord-eof-payload` 结尾，且直接子进程已被回收、缓存状态为 0。
- `local_coordination_git_eof_before_nonzero_exit_returns_none_and_reaps`（Linux）：同样先 EOF，放行后退出码 7；结果为 `None`，子进程已被回收且缓存状态为 7 —— 在 EOF 处立即 kill 的实现会得到 SIGKILL 而失败。
- `local_coordination_git_eof_before_deadline_kills_and_reaps_owned_child`（Linux）：EOF 后门控保持不动超过收集截止时间；结果为 `None`，返回前已完成回收，缓存状态信号为 `SIGKILL`。
- 平台门控：`local_coordination_git_drains_pipe_backpressure`（使用 `F_SETPIPE_SZ`）与上述三个用例仅在 Linux 运行；`local_coordination_guard_heartbeat_and_close`、`local_coordination_snapshot_is_read_only`、`local_coordination_clock_jump_is_not_death` 也标记为 Linux-only，因为注册需要 Linux 所有者适配；仅存储的行上限用例仍留在 Unix，非 Linux 的 `local_coordination_unsupported_registration_creates_nothing` 保留。

#### 第 5 轮：只读 `dcl sessions live`

`dcl sessions live [--json]` 在 `RuntimePaths` 构造之后、资源采样器/日志/环境变量/provider/会话数据库之前分派，只读渲染共享注册表：不注册参与者、不打开会话数据库、不连接 provider，也不受 `enabled=false` 影响。协调根按显式 `--coordination-dir` > `DEEPSEEK_CODE_COORDINATION_DIR` > 家目录共享根解析（环境变量由解析器显式读取以便测试优先级），相对路径锚定在 `-C` 之前的启动目录。空注册表是成功结果（`domain_id` 为 null、空分组），缺失存储不创建任何目录。JSON 输出为单个对象加一个 LF：`format_version=1`、无损 root（`encoding`/`base64`/转义 `display`）、`coverage`、分组内每个实例的完整 UUID、kind/role/worker/job 链接、owner pid/platform/boot/creation/generation、转义的 workspace 与 branch_ref、activity/start/heartbeat/proof/responsiveness。文本输出为纯行（`Local coordination:`、`Coverage:`、`No live local sessions.`、分组行、每个 owner 一行），ID 缩短为 12 个十六进制字符（前缀冲突时用完整 UUID），截断时追加 `More participants exist; output limited to 100.`。失败时 stderr 输出 `Local coordination unavailable: <busy|unsupported_schema|corrupt|missing_database|domain_mismatch|unavailable>. No state was reset.` 并以非零退出，不打印原始错误文本或错误链。

- `cargo test --locked -p deepseek-code-cli --test local_coordination_registry -- --nocapture`：9 个验收用例全部通过，另有 1 个默认忽略的 `lc_registry_helper`（子进程夹具，不算验收）。
- `sessions_live_empty_does_not_create_state`：不存在的协调根与数据目录下，文本与 JSON 均为空视图、退出 0，且两者都未被创建。
- `sessions_live_groups_frontend_and_worker`：启动两个真实 helper 进程（同一个已构建测试可执行文件的 `lc_registry_helper`，逐个等 ready）在同一 namespace/session 注册 frontend 与 executor，CLI `--json` 得到一组两个实例、owner PID 恰为两个 helper 的 PID、generation 不同且均为 `alive`；同一 session 换数据目录后成为第二组。helper 由 `Helper` 守卫 kill/wait，释放后各自 `close` 并写 `closed` 标记。
- `sessions_live_respects_root_precedence`：显式参数压过环境变量，环境变量压过默认根；无关的 `--data-dir` 与 `-C` 不改变根；相对覆盖以启动目录为基准。
- `sessions_live_redacts_and_escapes`：路径中的换行/ESC/非法 UTF-8 在文本里转义、在 JSON 中 base64 无损；API key/token/model 哨兵值不出现在 stdout 或 stderr。
- `sessions_live_errors_do_not_reset`：回滚日志 → `corrupt`、只有 domain 标记 → `missing_database`、更高 `user_version` → `unsupported_schema`；三者都非零退出、stderr 恰为该类别一行，证据文件与会话数据库均未被创建或修改。
- `sessions_live_accepts_non_utf8_coordination_dir_argv`（Linux）：`--coordination-dir` 的值是一个含 ESC、LF 与 0xFF 的原生路径，分别放在子命令前后两次运行；两次都退出 0 且 stderr 为空；JSON 以 `{` 开头、以 `}\n` 结尾且只含一个 LF，`root.encoding` 为 `unix_bytes`，base64 解码后与 `OsStr` 原生字节完全相等，转义 display 以 `coord-\x1b-\x0a-\xff` 结尾；文本恰好三行且首行以同一转义结尾；没有创建协调根、数据目录或其他条目。
- `sessions_live_invalid_native_root_error_is_bounded`（Linux）：以含 ESC/换行/0xFF 名字的普通文件为祖先传入原生 `--coordination-dir`；退出码 1、stdout 为空、stderr 逐字节等于 `Error: Local coordination unavailable: unavailable. No state was reset.\n`（没有 ESC、路径文本、额外换行或 panic）；证据文件内容与父目录条目集合不变，且不创建数据目录。
- `sandbox_bridge_exact_role_still_dispatches`（Unix）：以恰好 `__sandbox-bridge` 启动真实二进制并关闭 stdin；缺少 socket 操作数时退出 126、stdout 为空、stderr 仅为 `sandbox bridge: socket path missing\n`，证明私有角色在普通解析之前仍被识别。
- `sessions_saved_list_behavior_is_unchanged`：`sessions list` 仍是 JSON 数组、`sessions show` 仍是 `{session,messages}` 对象，且都不会创建协调状态。

#### 第 6 轮：CLI 与持久 Worker 注册生命周期（R1 设计收尾）

本轮把只读注册表接上真实参与者：`Session::open` 保持原签名，委托给 `open_with_registration(cli, paths, selection, resumed_id, mode)`；`mode=Automatic` 在全部初始化与 workspace 解析成功后读取 `settings.json` 的设置快照，仅当 `enabled && registry` 时解析根、观察 workspace 并注册；`mode=HostOwned` 供持久 Worker 宿主使用，避免同一会话出现第二行。注册失败只输出一次 `Local coordination registry unavailable: <category>. Session continues without peer visibility.`，不打印原始错误、路径或提示；禁用时既解析根也不创建心跳任务。

- 普通会话是 `cli/frontend`；经 `worker_directory` 验证为内部作业的会话是 `background_agent/executor`，`job_id` 只接受规范 UUID。`data_namespace` 与 `RegistryGuard::register` 都在有限的 `spawn_blocking` 中执行，异步注册线程只等待 `observe_workspace`。
- 持久 Worker 宿主在验证 worker 身份/生命周期锁之后、监听循环之前注册一个 `background_agent/executor` 宿主守卫，其 `worker_id` 是已验证的 Worker 实例 UUID，空闲与逐轮期间都保持；`Session::open` 用 `HostOwned`，宿主退出时先尝试 Session 关闭，再等待注册关闭（即使关闭报错）。恶意或过时的宿主进程被强杀后，其 Open 行由 owner proof 报告为 `Gone`。
- `resume_saved` 成功后以新会话 ID/workspace 注册新代际，并等待旧守卫关闭；失败时保留旧守卫。`clear` 同样替换行。`sync_workspace` 只在路径真实变化时调用 `request_workspace_refresh`，观察与栅栏写入仍由注册线程完成；每轮开始还会对同一路径请求一次刷新，使外部 checkout 的分支可见。`run_timed_turn` 周围用 RAII 只发布类型化的 `Active`→`Idle`（含错误与取消）。
- `main` 在 `-C` 之前捕获启动目录；仅在启用协调或显式 `--coordination-dir` 时把解析后的绝对根写入 `DEEPSEEK_CODE_COORDINATION_DIR`，所有已知宿主子进程（supervisor、Worker 宿主、SDK/远端子进程）继承它，注册不再依赖当前目录。正常退出在 `session.close().await` 之后调用 `close_coordination()`（即使关闭报错），远端交接在 `std::process::exit` 前也调用。注册租约与 PresenceLease、ExecutionLease 相互独立。

自动回归：

```bash
cargo test --locked -p deepseek-code-cli --test local_coordination_registry -- --nocapture
cargo test --locked -p deepseek-code-cli --bin dcl registry_resume_replaces_incarnation -- --nocapture
```

- `cargo test --locked -p deepseek-code-cli --test local_coordination_registry -- --nocapture`：14 个验收用例通过，另有 2 个默认忽略的进程夹具 `lc_registry_helper`、`lc_registry_lifecycle_helper`（不算验收）。
- `registry_disabled_real_session_creates_no_state`（Linux）：真实 CLI + 本地 SSE 夹具，`enabled:false`、显式 `--coordination-dir` 完成一次普通回合；退出 0、结果 JSON 正常、模型请求恰为 1 次，协调根不存在且没有注册行，stderr 不提协调。
- `registry_real_frontend_and_worker_share_group`（Linux）：真实 stream-json 前端先打开会话并保持空闲，随后 `ensure_worker` 启动真实持久宿主并提交带 `ROUND06_GATE` 的回合；在模型请求被夹具门控、回合未完成时读取注册表：同一 data namespace/session 恰好两个 Open 实例（`cli/frontend` 与 `background_agent/executor`，不同 PID/generation），executor 的 `worker_id` 等于宿主实例 UUID，没有第三行；放行后回合只发一次模型请求，宿主正常停止后两行都是 `Closed`。
- `registry_worker_survives_frontend_exit`（Linux）：同样的门控回合中由夹具进程终止前端（精确 PID）并回收；`registry_snapshot` 显示前端 `owner_proof=gone`、executor 仍 `alive`；`ExecutionLease::acquire` 仍被拒绝（现有执行租约未被绕过），后台作业仍未完成；放行后完成且只发过一次请求，宿主正常关闭，前端遗留的 Open 行仍可见为 `Gone`。
- `registry_resume_replaces_incarnation`（Linux，位于 `--bin dcl` 单元测试，见下）：成功 `resume_saved` 关闭旧行并以新 ID/workspace 产生新 `instance_id`；失败的恢复保留当前守卫与 Open 行；`sync_workspace` 改变 workspace 后，注册线程在 10 秒内把新 workspace 写入同一行；`close_coordination` 后最终行 `Closed`。
- `registry_failure_does_not_break_session`（Linux）：0700 根中放入回滚日志证据；真实回合在模型请求门控时，第二次 `ExecutionLease::acquire` 被拒绝；放行后原回合成功、模型请求恰为 1 次；stderr 恰有一行 `Local coordination registry unavailable: corrupt. Session continues without peer visibility.`；日志字节与根目录条目集合完全不变。
- `registry_worker_restart_has_new_owner`（Linux）：两个独立数据目录的会话先后启动/停止真实宿主，但都从同一继承根注册；旧行在正常停止后为 `Closed` 且不被重复行替代；新宿主具有不同 `instance_id`、不同 worker 实例 UUID、不同 owner generation，并与旧宿主属于不同 data namespace，证明根来自继承而不是数据目录。

`registry_resume_replaces_incarnation` 需要真实的进程内 `Session`/`resume_saved`，而集成测试只链接 core，因此放在 `crates/cli/src/session.rs` 的单元测试中，命令如上；其名称与断言仍按验收矩阵。纯黑盒的启动 `--resume` 只覆盖 `open_with_registration`，不能替代此用例。

未运行/未验证（不得由上述自动化推断）：真实 TTY 的交互式 frontend 交棒（本用例用空闲 stream-json 前端 + 真实宿主验证分组，未驱动 raw-mode UI 交棒）；B 运行中排队与撤回、D 中途关闭/`/continue` 挂接/恢复、E 取消与 Worker 清理的终端人工场景；活动 `Active`→`Idle` 的 5 秒心跳时序与 Drop 取消边界；`/clear` 的行替换；复用旧 Worker 属于另一协调根时的根不匹配展示；`busy`、`domain_mismatch` 与其它类型化存储错误经 CLI 的警告分类；100 个参与者截断；macOS/Windows 的原生进程证据与非 UTF-8 Worker workspace；只读/NFS 挂载、真实崩溃与 PID 复用竞争、权限受限的 procfs/pidfd、非回收 init 下的孙进程重新挂靠。

#### 第 6b 轮：启动根贯穿前端与子进程、初始化完成后再发布

本轮修正 review-06 的前两项：协调根只在启动边界决定一次，前端、重启、远程交接和 Worker 子进程共用同一个原生绝对根；注册推迟到初始化成功之后；所有正常退出路径都显式等待注册关闭。

- `local_coordination::{RootSelection, CoordinationContext, PreparedCoordination, prepare_startup}`：`prepare_startup` 在 `spawn_blocking` 中读取一次设置；只有 `enabled && registry` 或存在显式 `--coordination-dir` 时才用调用方传入的启动目录（`-C` 之前）解析根，绝不再读 `current_dir`。关闭且无显式覆盖时保持 `Deferred`（完全不做协调根查找）；解析失败保持 `Rejected`。每次尝试最多一条 `Local coordination registry warning/unavailable` 有界提示。`CoordinationContext::child_root` 对 Ready 返回精确原生字节，对 Deferred 返回 None，对 Rejected 返回空原生值（解析器会拒绝空覆盖，因此子进程不会回落到默认根）。只有启动边界调用 `export_child_root` 写入 `DEEPSEEK_CODE_COORDINATION_DIR`。
- `Session::open_with_registration` 不再注册，只记录 `RegistrationMode`；`activate_coordination(prepared)` 在启动选项（含 `--worktree` 的最终 workspace）与 SessionStart/初始化钩子成功之后发布；`HostOwned` 会话返回而不注册。`replace_coordination` 只在 `Automatic` 且启动决定的 `enabled` 为真时执行，因此“显式根 + 关闭”不会因为根存在就注册。`close_owned()` 先取 `Session::close` 结果，再等待 `close_coordination`，最后返回原结果；正常 teardown、`/restart`、voice 与远程交接都走它或显式 `close_coordination`。
- `persistent_worker::run_from_env` 先 `Listener::bind` 再 `register_worker`，并把监听循环包进 `let serve_result: Result<()> = async { … Ok(()) }.await`：绑定、accept 或普通循环错误都会汇聚到同一条 teardown，先尝试 Session 关闭，再等待宿主守卫关闭，最后按服务错误、Session 关闭结果的顺序返回。孤儿强制 `process::exit(1)` 与宿主生命周期锁不变。
- `/restart`、交互式远程交接和直接 `RemoteControl::Listen` 从 Clap 解析结果重建参数，跳过 `--coordination-dir`，子进程继承导出的原生根。工作目录、数据目录等原生参数保持为 OS 字符串，直接启动中 `--` 后的位置参数仍按文本传递。

自动回归：

```bash
cargo test --locked -p deepseek-code-cli --test local_coordination_registry -- --nocapture
cargo test --locked -p deepseek-code-cli --bin dcl registry_child_root_exports_the_native_value -- --nocapture
cargo test --locked -p deepseek-code-cli --bin dcl restart::tests -- --nocapture
```

- `cargo test --locked -p deepseek-code-cli --test local_coordination_registry -- --nocapture`：本轮 18 个验收用例通过，另有 2 个默认忽略的进程夹具（`lc_registry_helper`、`lc_registry_lifecycle_helper`，不算验收）。
- `registry_relative_override_uses_launch_root_for_frontend_and_child`（Linux）：从临时启动目录以 `-C <other project>` 和相对 `--coordination-dir coord-rel` 运行真实 CLI 完成一次本地 SSE 回合；启动根只在 `<launch>/coord-rel` 出现且唯一前端行正常 `Closed`，`<project>/coord-rel` 不存在；同步 SessionStart 钩子进程把继承的 `DEEPSEEK_CODE_COORDINATION_DIR` 写成原始字节，与同一绝对根逐字节相等。
- `registry_worktree_startup_publishes_final_workspace_before_first_turn`（Linux）：临时 Git 仓库提交后以空闲 SDK（stdin 保持打开）启动 `--worktree reviewer`；等待真实 `Worktree: ` 公告与一条 Open 注册行，首行 workspace 即链接工作树（不是源 checkout）、`branch_ref` 为 `refs/heads/worktree/reviewer`、`worktree_id` 存在，且模型请求为 0；关闭 stdin 后正常退出，同一行变为 `Closed`。
- `registry_failed_worktree_startup_publishes_no_row`（Linux）：PATH 中的 Git 包装器只让 `worktree add` 以唯一诊断和退出码 7 失败；真实 CLI 以非零退出并报出该诊断，模型请求为 0，协调根（含 owner 租约）完全不存在，证明发布发生在启动成功之后。
- `registry_failed_host_bind_publishes_no_row`（Linux）：按真实服务目录写入类型化 `worker.json` 与 `host.lock`，endpoint 指向一个已存在的普通文件；以 `DEEPSEEK_CODE_SESSION_HOST` 启动真实宿主，退出码 1、stderr 报 `Session host failed`，协调根不存在，endpoint 证据字节不变，证明绑定先于注册。
- `registry_child_root_exports_the_native_value`（CLI 单元，Linux）：Ready 的 child root 等于精确原生绝对字节，Rejected 的空值无法解析成默认根。 `launch_preserves_native_paths_and_positional_flag_text` 核对非 UTF-8 项目目录保持原生字节，协调根从 argv 剔除，并保留 `--` 后看起来像 flag 的位置参数。两个用例都不修改测试进程环境。
- `registry_resume_replaces_incarnation`（更新后的 CLI 单元用例）：`Session::open` 后断言没有注册行，再以 `prepare_startup` 的启动输入调用 `activate_coordination`；其后全部原有断言（成功恢复替换、失败恢复保留、workspace 刷新、最终 `Closed`）不变。

未运行/未验证（不得由上述自动化推断）：Linux TTY 交互式交棒（B 排队与撤回、D 中途关闭/`/continue`、E 取消与 Worker 清理）；直接 `RemoteControl::Listen` 与交互式 `/rc` 交棒的真实子进程端到端重启（本轮只验证 argv 过滤与单元级构建器输出）；voice 发送路径的真实音频端到端；`Deferred` 起步后在后续 incarnation 首次开启协调（留待 06c）；跨 `-C` 的 Worker 继承根的真实多进程重启；macOS/Windows 原生路径与非 UTF-8 参数。

#### 第 6c 轮：事务式切换与宿主自身刷新

本轮修正 review-06 的第 3、4 项：resume/clear 只有一个提交点，失败时旧会话与旧注册行保持一致；提交后的新代际按当前设置重新决定是否注册；持久宿主在接纳回合时刷新自身行，并拒绝非规范 Worker 身份。

- `session.rs`：`start_hooks` 拆为 `prepare_start_hooks(id, workspace)`（检查 `owns_lifecycle` 与 `lifecycle_events::check_configuration`，不启动任何钩子）和 `start_prepared_hooks(source)`（置位 `lifecycle_owned`、执行 SessionStart 并把失败记为 `SessionStart hook: {error:#}` 诊断）。`resume_saved` 在结束旧钩子之前完成目标钩子预检和以目标 ID 为准的 `session_permission_mode::set`（原先在 `self.id` 赋值之后执行的可失败写入）；`clear` 在结束旧钩子之前完成目标预检。两者此后只有字段赋值：workspace/provider/model/auto_route、请求录制策略、ID、等待状态，然后替换注册并（必要时）运行已预检的钩子。任何预检或权限写入失败都会在提交前返回，旧 ID、workspace、等待状态、provider、lifecycle 与注册守卫全部保持。
- `local_coordination.rs`：`prepare_startup` 与新增 `prepare_next_incarnation(context, data_dir)` 共用同一个 `spawn_blocking` 决策函数。每次切换只读一次设置并要求 `enabled && registry`；`Ready` 根按字节复用，`Rejected` 在本次启动内保持，`Deferred` 最多解析一次且只用保存的启动目录与原生输入。`export_session_child_root` 由 `main` 在启动边界调用。
- `session.rs`：`replace_coordination` 改为“先取旧守卫 → 用 `prepare_next_incarnation` 重新决定 → 发布新行（仅在 enabled 且 Ready 时）→ 显式等待旧守卫关闭”。禁用设置不会关闭仍在运行的会话；下一次已提交的切换会关闭旧行且不产生新行或新租约。`Session::coordination_child_root` 暴露上下文的原生启动值，`main` 在每轮 REPL 起点、重启与远程交接前重新导出（覆盖“Deferred 启动在后续代际首次开启”的场景）。
- `persistent_worker.rs`：`Request::Submit` 在 `begin_job` 成功之后、启动回合之前对宿主守卫调用 `request_workspace_refresh(worker.workspace)` 并置 `Activity::Active`；空闲心跳发现子进程发起的 idle→active 转换时也入队一次刷新。同一 workspace 路径换分支即可被观察到，不产生每回合注册。
- `local_coordination.rs::register_worker`：在 `enabled && registry` 决定之后、任何根/存储/租约工作之前要求 `canonical_uuid(&worker.id)`；不满足时恰好输出一次 `Local coordination registry unavailable: unavailable. Session continues without peer visibility.` 并返回 `Ok(None)`，宿主继续提供 IPC 与生命周期锁。

自动回归：

```bash
cargo test --locked -p deepseek-code-cli --test local_coordination_registry -- --nocapture
cargo test --locked -p deepseek-code-cli --bin dcl registry_ -- --nocapture
```

- `cargo test --locked -p deepseek-code-cli --test local_coordination_registry -- --nocapture`：21 个验收用例通过，另有 2 个默认忽略的进程夹具（不算验收）。
- `registry_host_owned_turn_refreshes_branch_without_duplicate_row`（Linux）：共享 workspace 在宿主启动前初始化为已提交仓库；宿主启动后初始 Open executor 行为 `refs/heads/main`；在同一目录切换到 `after` 后再放行被门控的提交，轮询同一 instance/generation 直到字节引用变为 `refs/heads/after`；workspace 字节不变、恰好一个 executor、无每回合新行、`ExecutionLease::acquire` 仍被拒绝（回合持有执行租约）；放行后只发一次模型请求，正常停止后该行 `Closed`。
- `registry_rejects_noncanonical_worker_id_without_stopping_host`（Linux）：按真实服务目录写入 `id=sentinel-free-form-worker-id` 的类型化 `worker.json` 与 `host.lock`，endpoint 为真实的空闲短 Unix 路径；真实宿主对 `Ping` 与 `Shutdown` 均正常应答、干净退出 0；stderr 恰好一行有界警告；协调根不存在，sentinel 从未进入注册输出。
- `registry_same_session_worker_restart_replaces_owner`（Linux）：同一数据目录/workspace/会话先后两次 `ensure_worker`；旧宿主行 Open→Closed 后新宿主注册；新 worker UUID、instance 与 owner generation 均不同，同一 namespace/session 内恰好一条新 Open 行且旧 Closed 行保留一次；结束并正常关闭。既有的双数据目录 `registry_worker_restart_has_new_owner` 保留为独立对照。
- `registry_resume_permission_write_failure_preserves_incarnation`（CLI 单元，Linux）：在会话数据库的 `peripheral_settings` 上为目标键安装 `BEFORE INSERT` 触发器；`resume_saved` 必须命中该写入并报出标记。断言旧 ID、workspace、provider、等待状态、守卫 instance 与 Open 行不变，且没有目标行；移除触发器后同一恢复成功，旧行 `Closed`、新 instance 与 generation 不同、显式关闭后新行 `Closed`。
- `registry_resume_hook_preflight_failure_preserves_incarnation`（CLI 单元，Linux）：旧 `lifecycle_owned` 为真，目标 `.claude/settings.json` 是目录使配置读取失败；要求在提交前报错，旧 ID、workspace、lifecycle 与守卫不变且无目标行；恢复该夹具后同一切换提交，SessionStart 钩子恰好执行一次。
- `registry_settings_off_prevents_resume_and_clear_incarnations`（CLI 单元，Linux）：resume 与 clear 各自使用独立的启用夹具；首次守卫 Open 后在磁盘把 `localCoordination.enabled` 置 false，未变动的会话仍 Open；完成切换后会话 ID 改变、旧行 `Closed`、没有新行/新 owner 文件/新守卫；重新启用后下一次切换恢复注册（阳性对照）；全程使用同一原始绝对根。
- `registry_resume_replaces_incarnation`（CLI 单元，Linux）：现为隔离父用例——它用 `std::env::current_exe()` 以 `--exact session::event_tests::lc_registry_session_workspace_helper --ignored --test-threads=1` 启动私有夹具进程并断言恰好运行该用例；原先的恢复/工作区/关闭断言全部移入该忽略辅助用例，cwd 变更只发生在它自己的进程里。

未运行/未验证（不得由上述自动化推断）：Linux TTY 交互式交棒（B 排队与撤回、D 中途关闭/`/continue`、E 取消与 Worker 清理）；`/restart` 与 `/rc` 的端到端重启（本轮只验证主流程重新导出与构建器输出）；voice 真实音频；`Deferred` 启动后在后续代际首次开启协调的端到端多进程证据（只有源码级重新导出）；`Rejected` 之后拒绝回落的路径；macOS/Windows 原生路径与 IPC；只读/NFS 挂载、真实崩溃与 PID 复用竞争。

#### 第 6d 轮：提交后钩子取消、print 启动校验与宿主活动去重

本轮修正 review-06c 的两个阻塞项，并移除 `Request::Submit` 中重复的宿主活动写入。

- `session.rs`：新增私有 `finish_committed_start_hooks(source)`，其内容为 `if let Err(error) = self.start_prepared_hooks(source).await { eprintln!("SessionStart hook: {error:#}"); }`。`start_prepared_hooks` 的唯一返回错误是 `tokio::select!` 的两个固定取消（`SessionStart hooks interrupted` 与 `SessionStart hooks cancelled`），普通钩子失败本来就只作诊断。`clear` 与 `resume_saved` 在提交（字段赋值 + `replace_coordination`）之后改用该辅助函数并返回成功结果；初始启动仍经 `start_hooks` 直接传播取消错误（`--resume ID` 首次启动的 source 字符串同为 `"resume"`，但走的是传播路径）。
- `main.rs`：把一次性运行的输入校验移到 `activate_coordination` 之前——`if !sdk_stream && single { ensure!(session.initial_hook_message.is_some() || !prompt.trim().is_empty(), "A prompt or piped stdin is required") }`，并删除 `initial_hook_message` 消费分支里的旧 `ensure!`。钩子提供初始消息的回合因此先通过校验、先发布，再发出唯一一次模型请求。
- `persistent_worker.rs`：`Request::Submit` 在 `active.compare_exchange` 与 `*current_turn.lock()` 之间不再写注册活动；`begin_job` 成功之后的 workspace 刷新与 `Activity::Active` 写入保持不变。

自动回归：

```bash
cargo test --locked -p deepseek-code-cli --test local_coordination_registry -- --nocapture
cargo test --locked -p deepseek-code-cli --test initial_user_message --test interrupt_signals
```

- `cargo test --locked -p deepseek-code-cli --test local_coordination_registry -- --nocapture`：24 个验收用例通过，另有 2 个默认忽略的进程夹具（不算验收）。
- `registry_clear_sigint_after_commit_returns_success`（Linux，真实 PTY）：在 `--ax-screen-reader` 交互式 dcl 中先等到一条 Open 前端行，发送 `/clear` 并等到 clear-only SessionStart 钩子（`matcher:"clear"`、`timeout:20`、由 `sh` 阻塞等待释放标记）写下的进入标记。钩子仍被持有时断言旧行 `Closed`，且恰好一条不同会话/实例的新 Open 行。用 `kill -s INT <pid>`（PID 来自该受控子进程，且发送前刚确认 `try_wait()` 为 None）后：转录中出现且仅出现一条 `SessionStart hook: SessionStart hooks interrupted`，以及精确的 `New session: <new>. Previous history retained: <old>`，没有 `Error: SessionStart hooks interrupted`；子进程仍在运行、新行仍 `Open`、模型请求为 0。随后释放钩子、发送 `/exit`，正常退出且同一新实例变为 `Closed`。该用例已通过“临时还原为 `start_prepared_hooks("clear").await?` 则超时失败”的反向验证，确认它确实覆盖该缺陷。
- `registry_print_without_prompt_publishes_no_incarnation`（Linux）：`--print`、`Stdio::null()` 标准输入、无位置提示、无钩子；退出码 1，stderr 含精确行 `Error: A prompt or piped stdin is required`，模型请求为 0，且在任何辅助代码打开存储之前观察到：协调根不存在、没有任何行（含 `Closed`）、没有 owner 文件。
- `registry_print_with_hook_prompt_publishes_incarnation`（Linux）：同样的 print 设置加 `--output-format json`，同步 SessionStart 命令钩子返回 `{"hookSpecificOutput":{"hookEventName":"SessionStart","initialUserMessage":"ROUND06_GATE"}}`，标准输入为空、argv 无提示；现有模型门控捕获到 `ROUND06_GATE` 时断言恰好一条 Open 前端行；放行后退出 0、同一会话一个成功 JSON 结果、模型请求恰好一次，且该实例 `Closed`。

未运行/未验证（不得由上述自动化推断）：`resume_saved` 提交后钩子取消的运行时用例（与 clear 共用同一辅助函数，仅有源码级证据）；初始启动钩子取消仍传播错误的运行时用例（仅保留源码中的错误传播调用）；挂起场景下的 `SessionStart hooks cancelled`（后台取消分支）；B/D/E 交互式交棒矩阵；与 main 的集成合并；macOS/Windows PTY 与信号语义；voice 真实音频。

### I4. 子进程启动参数与会话绑定

用隔离数据目录和 loopback mock。验证真实请求内的模型、预算和系统提示词，以及 SQLite 中的会话、目录与权限状态。

```bash
cargo test --locked --no-fail-fast -p deepseek-code-cli --test skills --test teams --test batch --test taskget_backends --test session_worker --test background_failure_diagnostics --test worktree_startup --test local_coordination_registry
cargo test --locked -p deepseek-code-cli --bin dcl restart::tests
cargo test --locked -p deepseek-code-core --test worker_environment
cargo test --locked -p deepseek-code-core --lib tools::external_agents
```

- slash skill fork 的系统提示词、追加提示词、shell 展开在实际请求中各出现一次，预算一致；子 ID 独立且不带父历史。模型调用 Skill 和 TeamSpawn 在进程内执行，核对子会话与父会话存储、真实请求和工具限制。
- `workflow_agents_use_the_snapshot_only_for_the_selected_model` 与 `workflow_with_a_virtual_context_keeps_only_matching_snapshots` 分别从普通模型和 `[1m]` 启动。实际请求中，默认 agent 继承 211 token；裸名和 `provider::model` 与父选择相同时也继承 211。检查大小写不同的虚拟上下文后缀，切换上下文或模型时使用配置的 4096。
- `external_dcl_from_a_background_worker_starts_and_resumes_its_own_session` 由真实 background supervisor 启动工具调用，验证嵌套 dcl 的首轮和恢复轮落在同一个新会话。父 Worker 的控制目录、模型快照和私有凭据不能变成外部工具的启动绑定。
- 持久 Worker 的 `persistent_engine_reuses_mcp_refreshes_settings_and_recovers_after_crash_without_replay` 对首次打开及复用的两轮发送不同的 [1m] 模型快照，确认实际模型、预算以及同一个 engine PID。另核对取消和冷重建不会重放。
- Linux 真实 PTY 使用相邻开发支持目录 `launch-binding-20261119/pty_repro.py`。对构建出的 `target/debug/dcl` 逐一运行 `remote-listen`、`remote-resume`、`handoff`、`restart`、`restart-argv`、`batch`、`markdown-workflow` 和 `background-fork`。命令为 `python3 pty_repro.py --binary /path/to/dcl --out /path/to/evidence SCENARIO`。直接 listen 从相对 `-C` 和相对 `--data-dir` 启动；无 `-C` 的直接 resume 应采用会话的保存目录；handoff 和 restart 检查变更后的模型、权限、标题及原项目和可信目录；restart-argv 使用与系统提示词选项值相同的位置参数。Batch 检查 planner 与五个隔离 unit 的预算、CLI-only deny 导致五次拒绝且无写入；Markdown workflow 和后台 fork 各使用一个新会话及原项目和历史；标题辅助请求单独记数。

- Linux PTY 的 Round 3 回归脚本在相邻开发记录目录的 `round3/launch_pty.py`。使用 `python3 -B review-01-evidence/run_owned.py python3 -B round3/launch_pty.py /absolute/path/to/dcl CASE evidence-dir`。对 `reload`、`keep-overrides`、`discard-overrides`、`switch-provider`、`handoff-keep`、`handoff-discard` 比较两个 loopback 端点的实际协议、凭据来源、模型和预算；检查 `/model` 与 `/permissions` 的活动选择，以及替换进程是否额外导出了磁盘凭据。
- 对 `remote-long`、`remote-equals`、`remote-short`、`remote-attached`、`remote-short-equals` 检查直接恢复到保存的项目且首屏无横幅和恢复提示；`remote-fresh` 检查全新启动横幅，`remote-continue`、`remote-c` 检查继续最近会话时保留横幅及恢复提示。普通终端的 `remote-long-native` 和 `remote-fresh-native` 在首个输入提示前检查滚动缓冲区清屏序列。

#### I4. 持久 Worker 的提供商绑定

- 在 Linux 原生终端运行开发记录中的 `round4/worker_transfer.py DCL handoff OUT`。先创建输出目录。第一个请求使用临时地址和启动密钥，通过 `/model` 重新选择内置提供商，再运行 `/remote-control listen`。检查两轮来自同一 Worker PID，第二轮只命中保存的地址，鉴权头使用新的夹具密钥。用 `switch`、`key`、`resume`、`restart` 检查直接切换、同端点换密钥、断开后恢复和重启。检查第二轮 hook 的子进程也看不到旧的 `API_BASE_URL`。`cloud-resume` 在回环服务上检查 Worker 使用新前端的 Cloudflare 账户路径。
- 核心库运行 `provider_cache_rebinds_bare_and_qualified_models_on_update_reload_and_delete`、`retained_child_keeps_its_launch_binding_and_new_children_use_the_owner_selection`、`retained_child_keeps_native_model_transport_after_owner_switch` 和 `sdk_agent_invocation_rebinds_and_its_retained_executor_rejects_rotation`。覆盖同 ID 的地址或密钥变更、删除提供商、显式与空闲子执行器共享的注册表和 SDK 显式模型选择。已失效的子执行器返回 `needs_resume`，不发送请求。
- CLI 单元运行 `resume_another_provider_and_sdk_agent_use_current_transport` 和 `worker_cloud_binding_replaces_and_removes_only_adapter_inputs`。集成运行 `--test async_rewake` 与 `--test session_worker owned_child_wakes_without_parent_turn`。前端改变绑定后，即使异步 hook 已完成，在接受新绑定的显式回合前也不能用旧 Worker 发起请求。未切换绑定时仍能唤醒子任务和 hook。

#### I4. 会话提供商绑定代际与旧版 Worker

- 运行 core 单元测试 `generation_changes_refresh_active_and_pinned_consumers_and_reject_late_submits`。每次切换后检查当前选择和显式 SDK 子代理各自的地址、密钥、模型预算，并拒绝过时代际和无效选择。
- 在 Linux 下使用开发记录 `launch-binding-20261119/round5/verify-wire.sh`。它在独立 HOME、数据目录和回环提供商上生成 `idle`、`mid-turn`、`nested`、`legacy` 和 `failed` 场景，逐次检查 SQLite 代际以及实际 HTTP 地址、模型和 Authorization。旧 Worker 的原回合可以继续；终端必须显示切换待生效，后续新回合必须使用新提供商。失败的模型更新必须保留代际，并允许原绑定继续触发空闲 hook。
- 同一脚本检查预热 Worker 中的外部 DeepSeekCode、SDK 指定提供商的环境凭据、非 UTF-8 Bedrock 凭据路径、升级前后忙碌 Worker 的重新连接以及失败切换后断开终端的异步唤醒。每个子场景在 subreaper 下运行。
- 同一代际矩阵还覆盖旧 Worker 退役前尝试提交新提示、观察端退出、显式取消、同一会话的单回合 Worker，以及工具调用往返期间 OpenAI 和 Anthropic 的双向切换。检查后一请求同时保留工具调用 ID 并使用新端点、新鉴权和新模型。
- 运行 core 单元测试 `binding_wire_preserves_runtime_model_options_without_persisting_them`、`binding_preserves_alias_and_virtual_context_across_generations`、`pending_legacy_generation_rejects_new_turn_and_child_delivery_until_retirement` 和 `toolscript_binding_is_frozen_at_launch_and_stays_out_of_job_records`。ToolScript 的真实 CLI/子进程用例为 `cli_toolscript_repeated_calls_keep_history_across_model_rounds_and_reopen`。

#### I4. 并发终端的绑定写入

- 运行 core 单元测试 `session_binding::store::tests`。检查过期写入被拒绝、相同有效配置不推进代际、只读恢复保留正在退役的 Worker 预约，以及激活失败后原配置仍可用且失败代际永不复用。
- 开发记录 `launch-binding-20261119/round6/review04.sh` 覆盖过期终端的重启和远程交接、fast 模式、第三个观察终端、旧 Worker 激活故障以及无变更时的空闲唤醒。核对回环端点、模型、测试密钥和 SQLite 状态。配合 `review03.sh`、`lifecycle.sh` 和原版 `cli_creates_spawns_and_disbands_a_team_standalone` 检查进程边界、11 个生命周期场景和成员的首条任务消息。

#### I4. Read-only relaunch and rejected binding changes

Run `cargo test --locked -p deepseek-code-cli --bin dcl -- session::binding_tests restart::tests` and the core `session_binding::store::tests` selection. Cover a previous-version relaunch with explicit arguments, a late token-cap race, a rejected clear and a concurrent interactive resume. Replay the six review-05 findings with the records' Round 8 probes. Check the actual private endpoint, key and token cap after restart and handoff, including a third observer. Verify that two rejected clears leave the original conversation alone and that the requesting terminal displays the failed activation and restored selector before another submission.

For a rejected launch selector, verify the launch prompt reaches neither topic naming nor the primary request, then submit separate input to the winner. Exercise picker effort and an activation rollback while the requester is inside a chooser with the review-06 Round 9 probes. Run `session::binding_tests` in the CLI and `session_binding::store::tests` in core, including the addressed receipt after a peer publishes again.

For unstamped worker submissions, run `unstamped_worker_adopts_the_authoritative_revision_without_publishing`, `session_worker::added_directory_survives_worker_resume_without_granting_other_sessions`, and `registry_worker_survives_frontend_exit` and `registry_real_frontend_and_worker_share_group` in `local_coordination_registry`. Exercise retained-child routing with `session_messages::resident_owner_resumes_same_child_with_original_provider_after_switch`. Verify the original child’s endpoint and key across the switch and the new child’s endpoint and key after it.

### I39 提示建议与主会话缓存

- `cargo test --locked -p deepseek-code-core --lib suggestion::` 验证五分钟时限、缓存未命中、各协议追加消息的格式，以及独立文件的替换、接受、历史校验和删除。`prompt::tests::prune_removes_old_files_and_keeps_recent_ones` 同时检查 30 天清理。
- `cargo test --locked -p deepseek-code-cli --test sdk prompt_suggestion` 使用本机 HTTP mock。对比 Chat、Anthropic Messages 和 Responses 两次 POST 的原始字节，只去掉新追加的一条用户消息。检查建议的工具调用被丢弃、设置关闭后跳过、恢复只发一条事件且无 HTTP 请求、其他进程续聊后旧建议失效。
- 用交互终端连接本机 mock，主回复报告缓存命中。确认输入框出现灰字，退出并恢复同一会话，再次出现同一条灰字且请求计数不变。按 Tab 接受后退出，再次恢复应无灰字。检查会话消息只含用户实际提交的内容和主回复。

I39 第二轮：在本地 HTTP 服务上挂起上轮建议，再从同一 SDK 或另一进程提交新回合。下一回合可立即进入；返回 503 后不得重发旧前缀。Linux 单回合前台 Worker 在建议应答前应完成前台任务并释放租约。以目录内置 ID 指向自定义 Responses/Gemini 地址时，零缓存读取只发主请求。用 `cargo test --locked -p deepseek-code-cli --test sdk prompt_suggestion` 和 `cargo test --locked -p deepseek-code-core --lib suggestion::` 检查。记录目录的 Round 2 脚本另覆盖首发前等待录制锁、域名映射与跨进程切换绑定。

### P Python 工具

自动检查：

```bash
cargo test --locked -p deepseek-code-core --lib tools::python::tests
cargo test --locked -p deepseek-code-core --test python_tool
cargo test --locked -p deepseek-code-core --lib python_settings
cargo test --locked -p deepseek-code-core --lib permissions::patterns::tests
cargo test --locked -p deepseek-code-core --lib python_output_consumes
cargo test --locked -p deepseek-code-core --lib python_gets_the_sandbox_policy
cargo test --locked -p deepseek-code-core --test advanced_tools
cargo test --locked -p deepseek-code-cli --bin dcl python_approval
cargo test --locked -p deepseek-code-cli --bin dcl tool_previews_are_bounded
cargo test --locked -p deepseek-code-cli --bin dcl shell_configuration_is_shared
```

手工/真实模型检查：

1. 用 `ToolLoad` 加载 `Python`，下一次请求才调用。直接调用应被拒绝；两个开关都关时目录、加载和调用都不出现。新会话提出纯 Python 片段/数值实验时，模型应先 `ToolLoad {"names":["Python"]}`，而不是用 Bash heredoc 或 `python -c`；`python -m`/带参数的脚本仍走 Bash。
2. `print(1)` 得到 `stdout:"1\n"`、`exit_code:0`、`script_saved:false`，且没有 `script_path`。检查 `__file__` 是真实 `.py` 路径，stdin 为空时 `input()` 立即得到 EOF。
3. `env` 中的变量在 `os.environ` 可见；`PYTHONPATH` 能导入工作区模块，工作区始终在 `sys.path` 中。非零退出码和 stderr 原样返回。
4. `save_name:"python/script/001.py"` 写到 `<workspace>/temp/python/script/001.py`，自动建目录、可覆盖旧文件，结果含 `script_saved:true` 与绝对 `script_path`；重复执行覆盖内容。目标为符号链接时拒绝。
5. 指定 `interpreter` 或 `conda` 时结果回显实际路径；同时传两者报错。`python.path` 可通过 `dcl config set python.path ...` 设置，`dcl config show` 可见，空值清除。
6. 前台超时后进程被清理并返回 `timed_out:true`；后台任务返回 task id，`TaskGet` 能读到输出，`TaskStop` 能停止。后台未保存的临时脚本不会在前台清理阶段删除。
7. 开启沙箱（Linux/macOS）后 Python 被解释器级包装，网络允许列表和主机审批与 Bash 相同；Windows 上明确报错。计划模式拒绝 Python；`Python(*)` 和裸 `Python` 放行任意代码，`Python(import *)` 只按前缀匹配。
8. 模型预览按 `interpreter:`（仅与请求不同或有工作区虚拟环境覆盖时）→ `script:` → `Exit code:` → 小写 `stdout:`/`stderr:` 顺序显示，路径是单反斜杠，不落入 `Metadata:`。
9. CLI 审批框显示完整代码、`env`、`cwd`、`interpreter`/`conda` 和 `save_name` 目标；不提供 Allow for Session。工具调用摘要显示 `Python(description | 首行代码或保存路径)`，不再只有描述。`/permissions allow Python(*)` 后的调用不再询问。

### I42. Experience 索引与变更提醒

改动范围：`crates/core/src/experience.rs`、`prompt/mod.rs` 的 `SessionReminders.experience`/`experience_for_session`/`changes_reminder`、`session_files.rs`、`engine_core.rs`、`delegation.rs`、`crates/cli/src/config.rs`、`crates/cli/src/settings_dialog.rs`。

- `cargo test --locked -p deepseek-code-core --lib -- experience prompt`：
  - `experience::tests` 验证 frontmatter 解析、64 KiB/8 KiB 上限、根与一级子目录扫描、三级目录不扫描、同名跨根去重、确定性排序、`- 相对路径 | 名称: 描述` 行格式、正文不进入索引、凭据启发式跳过，以及 `experience.enabled` 默认 true 与持久化。
  - `prompt::tests::first_message_reminders_keep_the_stable_order` 断言首条消息提醒按 `user_instructions → standalone_frontend → project_instructions → resident_indexes → experience → loadable_tools → available_skills → environment → date → git → skills` 顺序输出（Experience 位于 MEMORY 索引之后、loadable_tools 之前；CLI 前端说明位于用户指令之后、项目指令之前；environment 位于 available_skills 之后、date 之前）；`experience_*` 断言首请求冻结、内部草稿工作区与关闭开关都冻结 `None`；`changes_reminder_reports_*` 断言新增/删除各只提醒一次。
  - `forget_*` 与 `prune_*` 断言 `.experience.json` 和 `<session>.experience.names.json` 纳入会话删除与 30 天清理。
- `cargo test --locked -p deepseek-code-cli --test experience`：OpenAI 与 Anthropic 各跑一遍。断言首条用户消息只有一个 `# Experience`，根与 `docs/.agents/experience/` 条目在、`docs/sub/.agents/experience/` 不在、行以相对路径开头、正文与无效文件标记不在；恢复后首条消息逐字一致；新增 `gamma` 只出现一次 `New experience:`，删除 `beta` 只出现一次 `Removed experience: beta`；关闭开关的新会话没有块，重新打开恢复。
- `cargo test --locked -p deepseek-code-cli --test peripheral config_memory_switch`：断言 `config show` 的 `experience.enabled` 默认 true，`config set experience.enabled false|true` 可读回；`/config` 的 Experience 行读取同一设置。
- 手工终端：在含 `.agents/experience/` 的临时工作区连本地 mock，确认首条消息顺序、`/config` 开关、恢复同一会话和压缩后只有一份块。Windows 与 Desktop WebView 未在本机验证。

已知平台问题：macOS 上 `cargo check -p deepseek-code-cli --all-targets` 会在 `local_coordination_registry` 失败（`private_root` 使用 `#[cfg(unix)]`，CoordinationPaths 导入使用 `#[cfg(target_os = "linux")]`）。这是既有问题，与本次改动无关；本次用 `--bin dcl --test experience --test peripheral` 检查。

### I43. 内置资源监控（CLI 与 Desktop 接入）

改动范围：`services/resource_monitor.rs`、`native_engine/engine_core.rs` 与 `native_engine/tool_loop.rs` 的告警注入、`crates/cli/src/session.rs` 的监控启动与工作区同步、`src-tauri/src/lib.rs`、`src-tauri/src/bridge/mod.rs`、`src-tauri/src/bridge/routes/chat.rs`、`src-tauri/src/commands/mod.rs` 的 Desktop 接入。

自动回归：

```bash
cargo test --locked -p deepseek-code-core --lib services::resource_monitor
cargo test --locked -p deepseek-code-core --test resource_monitor
cargo test --locked -p deepseek-code-core --lib native_engine::engine_core::resource_monitor_tests
cargo test --locked -p deepseek-code-cli --bin dcl resource_alert_renders
cargo test --locked -p deepseek-code-cli --bin dcl resource_alerts_serialize
cargo test --locked -p deepseek-code-cli --test resource_monitor
cargo check --locked -p deepseek-code-desktop --all-targets
```

`resource_alert_renders` 覆盖 `worker_frontend` 的 `● System resource warning: ...` 提示；`resource_alerts_serialize` 覆盖 stream-json 的 `kind/summary/details`。核心用例 `stale_undrained_alerts_expire_after_ttl` 与 `preparing_a_turn_updates_the_monitor_workspace` 分别覆盖恢复后陈旧告警清理，以及 EnterWorktree/ExitWorktree 后监控的磁盘目标跟随新工作区。

手工终端（必须人为压低阈值；真实主机负载触发无法在自动用例中稳定复现）：

1. 在隔离数据目录的 `<data>/config/settings.json` 写入 `{"resourceMonitor":{"enabled":true,"intervalSecs":2,"memoryAvailablePercentThreshold":99.9,"memoryAvailableMinBytes":1099511627776,"memoryConsecutive":1,"cooldownSecs":0}}`，启动 `dcl --data-dir <data>` 并发送普通消息。下一轮请求必须包含 `<kind>system_resource</kind>`、实际摘要和可执行的 `details`，终端显示 `● System resource warning: ...`；冷却期内同一资源只注入一次。
2. 同一场景加 `--print` 或 `--output-format stream-json`：出现 `{"type":"resource_alert","kind":"memory","summary":...,"details":...}`，其他事件顺序不变。
3. 空闲 REPL 等待数个采样周期：不得发起请求、不得显示提示；随后发送消息，告警只在该回合注入。两个同时空闲的 `dcl` 进程都不能被资源告警唤醒。
4. 将 `enabled` 设为 false 或设置 `DEEPSEEK_CODE_RESOURCE_MONITOR=0` 后不再采样。恢复会话（resume）和 EnterWorktree/ExitWorktree 后，监控的磁盘目标应跟随新的工作区。
5. Desktop 用同一配置对话时，Bridge SSE 应出现 `resource_alert`。本次未运行真实 Desktop 窗口，Windows 亦未验证。
