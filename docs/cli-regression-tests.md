# dcl 回归测试清单

修改 `crates/cli`、`crates/core` 后，agent 必须读取本文，确认改动没有影响其他功能。修改完成的标准不是“新功能能用”，而是下面对应的检查都做过，并如实汇报结果。

## 使用步骤

1. 用 `git diff --name-only` 确定改动范围，在“按改动范围选择场景”中找到要跑的场景。
2. 跑“必跑命令”。出现失败时，先在未修改的代码上跑同一个测试，区分是这次引入的还是原有问题。
3. 在真实终端里跑选中的场景。只改了不影响界面的内部逻辑时，可以只跑相关测试，但要在汇报中说明。
4. 新修的问题要在本文补一个场景，避免以后再次出现。
5. 汇报时说明跑了什么、结果如何、哪些没有验证（例如 Windows）。没跑的不能写成通过。

## 必跑命令

Linux 上先加载本机构建环境（`source ~/.local/dcl-build-env.sh`）。Cargo 命令串行执行。

```bash
git diff --check
cargo test --locked --no-fail-fast -p deepseek-code-core --lib --tests
cargo test --locked --no-fail-fast -p deepseek-code-cli
cargo check --locked -p deepseek-code-desktop --all-targets
```

加 `--no-fail-fast`，否则第一个失败的测试程序之后的测试都不会运行，结果会被漏掉。

## 已知失败

目前没有。全部测试应当通过，出现失败时先在未修改的代码上跑同一个测试，再判断是否由这次改动引入。

## 真实终端的测试方法

真实终端指在伪终端里启动 `dcl`，发送按键，再检查屏幕内容。Linux 上使用开发目录的工具：

- `runs/ux/drive.py`：按 JSON 步骤驱动 `dcl`，用开发目录 `.venv` 里的 Python 运行，才能得到真实光标位置（鼠标点击依赖它）。
- `runs/ux/snap.sh`：把录制内容渲染成最终屏幕。
- `runs/ux/dcl.sh`：用独立的 `DATA`、`PROJ` 目录启动。测试前先 `cargo build --locked -p deepseek-code-cli`，`cargo test` 不会更新它运行的程序。

注意事项：

- 每轮用新的数据目录，测试结束后结束自己启动的进程。
- `until` 等待的文字如果也出现在自己输入的提示词里，会立刻匹配成功，这时后面的输入会变成运行中发送。等待模型回复时要选提示词里没有的文字，或用固定等待。
- 不要用 `pkill -f "某段文字"`，它会连当前 shell 一起结束。
- `--resume` 时如果 worker 还活着，不能带 `--permission-mode`，这是有意的限制。

## 场景

### A. 输入框换行

1. 空闲时输入文字，按 `Ctrl+J`，再输入文字：显示两行，没有发送。
2. 空闲时按 `Shift+Enter`、`Alt+Enter`、行尾 `\` 加 `Enter`：都换行。
3. 模型运行中重复以上操作：换行且不发送，输入框真正分行显示，内容很长时最多显示 8 行并随光标滚动。
4. 多行内容中按 `↑`/`↓` 在行间移动光标；到第一行或最后一行时才回到历史记录。
5. 普通 `Enter` 仍然发送，斜杠菜单打开时 `Enter` 仍然选择菜单项。
6. Windows 上 `Shift+Enter` 依赖输入读取方式：ConPTY 按 Win32 输入记录发送带 `SHIFT` 的回车，而 `ENABLE_VIRTUAL_TERMINAL_INPUT` 会把它重新编码成普通回车，直接发送。`crates/cli/src/terminal/paste_input.rs` 的 Windows 读取器保持记录模式，不要改回 VT 输入；改动后要在真实 ConPTY 里复测本条和多行粘贴。

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
2. `disabled_tools`：模型看不到该工具，强行调用被拒绝。
3. `tool_descriptions`：模型读到修改后的说明。
4. `system_prompt`：`replace` 和 `append` 生效，环境信息仍然存在。
5. 文件语法错误时这一轮报错退出；删除文件后恢复默认。

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

## 按改动范围选择场景

- `terminal/editor.rs`、`terminal/live.rs` 的按键处理：A、B、E1、E2、E3
- `terminal/live.rs`、`terminal/scrollback.rs`、`terminal/change_view.rs`、`terminal/tool_view.rs` 的显示：B、C、E1
- `repl.rs`：C4、D1、D2、D6、D7
- `worker_frontend.rs`、`background_commands.rs`：B、D、E3、E4、F
- `memory.rs`、`display_commands.rs`、`crates/core/src/memory`：G
- `services/customization.rs`、`prompt/`、`native_engine/tool_loop.rs`：H，以及 SDK 测试
- `tools/cron_jobs.rs`、`tools/wakeup.rs`、`main.rs` 的空闲循环：I
- `terminal/picker.rs`：F，以及 `/resume`、`/rewind`、`/mcp` 等使用选择列表的命令
- `session.rs`：B、D、E
- `services/session_worker*`、`services/background_sessions.rs`：D、E4
- `runtime.rs`、`db/message_repo.rs`（发给模型的历史）：D2、E5，以及 SDK 测试
- `services/input_queue.rs`：B、E4
- 权限相关：E2，以及 `added_directory_survives_worker_resume_without_granting_other_sessions`（`bypass` 模式可以读工作区外的文件，其他模式受目录范围限制）
- Provider、模型请求：E1、E6
- 其他范围：至少跑 E

## 尚未验证

- Windows 和 macOS 上还没有跑过这份清单。`Ctrl+J`、鼠标点击和进程退出判断都和平台有关，改动相关逻辑后要在 Windows 终端确认。
