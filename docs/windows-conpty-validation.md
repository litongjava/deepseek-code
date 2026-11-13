# Windows ConPTY 人工验证

本文说明当前实现的验收方法，不是某次 Windows 运行通过的证明。除另行指定工作目录外，命令从仓库根目录执行；使用同一次检出的源码、夹具和构建产物，记录实际提交及二进制 SHA256。不要使用同名的已安装程序替代本次构建。

## 自动覆盖与仍需人工验证的范围

[平台工作流](../.github/workflows/platform-tests.yml) 在 `ci/**` 分支 push 或手动 `workflow_dispatch` 时执行；普通 main 推送不会触发它。Windows 自动运行 core 的 `tools::pty_bash`、`tools::conpty`、`tools::shell_input` 和 CLI 的 `conpty_worker`。检查对应提交的 Windows 日志及 `runtime-tests-windows-latest` artifact；配置了测试不代表某次运行已通过，必须记录实际测试名、数量和退出码，0 项不算通过。

[conpty_worker.rs](../crates/cli/tests/conpty_worker.rs) 的 Windows 自动覆盖包含以下 8 项（完整测试名带 `windows_tests::` 前缀）：

- `model_conpty_console_write_resize_resume_timeout_stop_tree_and_records`：真实控制台、回显、resize、同一持久 worker 跨轮/resume、TaskGet 等待超时不停止任务、外 session/同 session 外进程输入拒绝、plan 输入拒绝、TaskStop 子孙清理、SQLite 独立调用 ID。
- `model_conpty_eof_and_invalid_input_have_no_effect_then_normal_exit`：EOF 与非法参数无副作用，正常退出和清理。
- `model_conpty_plan_denies_start_without_marker_or_task`：plan 拒绝启动，不创建任务或标记。
- `model_conpty_owner_hard_exit_kills_gate_shell_and_descendant`：强杀确切 owner 后由 Job Object 清理 gate、shell 和后代，不代表 SQLite 终态已修复。
- `model_conpty_exit_code_259_is_failed_not_permanently_running`：等 `EXIT_259_READY` 后通过输入释放退出，20 秒内得到 failed、精确退出码 259 和清理结果。
- `model_conpty_unsupported_cmd_and_pipe_never_run_the_command`：明确拒绝 cmd PTY 和 Windows pipe，命令未执行。
- `model_conpty_initialization_failure_is_terminal_without_starting_command`：初始化失败落 failed，命令未运行。
- `model_conpty_configured_git_bash_reuses_shell_arguments_and_cwd`：默认 Git Bash 的参数、输入及工作目录。

core 另有 Job Object、同步写超时/取消、DSR 光标查询分片、环境/argv/cwd 和参数边界测试。macOS 的同一 CLI 测试目标只验证明确拒绝 PTY，不能计作 ConPTY 支持。

**仍需人工验证**：真实 Windows Terminal 的输入、界面展示、退出/恢复操作，以及 Desktop 的工具结果展示、窗口与 owner 生命周期。下面复用部分自动场景，但人工结果必须独立记录，不把 worker 测试等同于 UI 验收。

## 获取与构建（Windows PowerShell）

准备独立检出及新的证据目录，使用已有 Rust、Python 3、Git Bash 和 Windows PowerShell。记录 `git rev-parse HEAD`、`git status --short`、Windows/PowerShell/Rust/Python 版本和架构。不要把真实配置或凭据复制到测试目录。

```powershell
$repo = (Resolve-Path .).Path
Get-Content -LiteralPath .\docs\windows-conpty-validation.md -Encoding UTF8
Get-Content -LiteralPath .\crates\cli\tests\fixtures\conpty-console.ps1 -Encoding UTF8
Get-Content -LiteralPath .\crates\cli\tests\fixtures\conpty-manual-mock.py -Encoding UTF8
$env:RUST_TEST_THREADS = '4'
$env:RUST_MIN_STACK = '16777216'
cargo test --locked -j 2 -p deepseek-code-core --lib tools::pty_bash -- --nocapture
cargo test --locked -j 2 -p deepseek-code-core --lib tools::conpty -- --nocapture
cargo test --locked -j 2 -p deepseek-code-core --lib tools::shell_input -- --nocapture
cargo test --locked -j 2 -p deepseek-code-cli --test conpty_worker -- --nocapture
cargo build --locked -j 2 -p deepseek-code-cli
Get-FileHash .\target\debug\dcl.exe -Algorithm SHA256 | ForEach-Object { $_ | ConvertTo-Json -Compress }
```

逐条执行，每条保存新日志并立即记录 `$LASTEXITCODE`；任一失败停止。PowerShell 写文本日志显式使用 `-Encoding UTF8`，不要把非零退出码被下一条命令覆盖后的结果记作通过。这些 Windows 命令不应在 Linux 上运行后以空过滤器结果代替。

## 实现边界（验收前先读）

- Bash `interaction:"pty",run_in_background:true`：Linux PTY / Windows ConPTY；不允许 `detach:true`。rows 1..500 默认 24，cols 1..1000 默认 80；真实 program/args/env/cwd 来自正常 Bash shell 选择。Windows 默认仍为现有 Git Bash 选择逻辑；可明确选 `shell:"powershell"` 或 `"pwsh"`。
- Windows `eof:true` **尚不支持**，与 data 混用时整次拒绝、0 字节。ConPTY 不提供 Unix canonical VEOF；Ctrl+Z/Ctrl+D 的意义由应用决定，可用 data 显式发送，不能当作通用 EOF 或永久关闭。用 `exit\r` 让本夹具正常结束；用 TaskStop 停止整个任务树。
- Windows pipe、macOS PTY、配置 sandbox 的 PTY、显式 Bash timeout、Monitor、Windows WSL/cmd PTY 均拒绝；不得记作支持。TaskGet timeout 只限制等待。
- TaskInput 的 resize 必须独立调用，不能和 data/eof 混用；单次 data 最多 64 KiB（UTF-8 字节），timeout 默认 10000 ms、范围 1..60000 ms。`bytes_written` 只表示操作系统已接受的字节，不代表应用已消费；Windows 写错误的 `bytes_written_lower_bound:true`、`outcome:"unknown"` 表示还可能有未报告的部分写入，不自动重放。
- Windows 输入只在**最初 owner 进程**内路由。持久 worker 跨轮/resume 可以继续写；另一 session 拒绝；另一个进程即使知道同 session 的 ID 也明确拒绝，不依赖可复用 PID 或心跳来授权输入。TaskGet 和 TaskStop 沿用会话持久记录/取消标记。
- portable-pty 的 ConPTY gate 在运行用户 shell 前加入不可 breakaway 的 Job Object，并关闭 gate 的 job 句柄；owner 最后句柄关闭会由内核终止任务树。强杀后的日志尾部和 SQLite 终态不承诺完成：旧任务会呈现 unresponsive，不能宣称 completed/cleanup_complete；Linux supervisor 的 owner-loss 持久化 reconcile 尚未移植到 Windows。

## 真实终端中的模型调用（本地 mock，无真实凭据）

准备新的证据目录、空工作目录和数据目录，不放在包含祖先 AGENTS/CLAUDE 配置的项目中。使用仓库夹具 [conpty-console.ps1](../crates/cli/tests/fixtures/conpty-console.ps1) 和 [conpty-manual-mock.py](../crates/cli/tests/fixtures/conpty-manual-mock.py)。Python mock 只监听回环地址，使用标准库，以新建模式写 UTF-8 日志；已有日志路径会报错。它按提示产生固定工具调用，不连接真实模型；日志包含完整夹具请求，请只在隔离会话中使用。

```powershell
$repo = (Resolve-Path .).Path # 在仓库根目录执行
$case = Join-Path $env:TEMP ('conpty-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path $case, "$case\workspace", "$case\data" | Out-Null
Write-Output $case # 记录这个实际路径，供其他测试窗口使用
Copy-Item "$repo\crates\cli\tests\fixtures\conpty-console.ps1" "$case\workspace\fixture.ps1"
Get-FileHash "$repo\crates\cli\tests\fixtures\conpty-console.ps1", "$repo\crates\cli\tests\fixtures\conpty-manual-mock.py" -Algorithm SHA256 | ForEach-Object { $_ | ConvertTo-Json -Compress }
# 仅清除这些隔离测试终端继承的变量。
Get-ChildItem Env:DEEPSEEK_CODE_* | Remove-Item
python "$repo\crates\cli\tests\fixtures\conpty-manual-mock.py" --port 18765 --log "$case\model.jsonl"
```

保留 mock 终端，记录其精确 PID。PowerShell 窗口不共享变量；第二、第三窗口先把 `$repo`、`$case` 设成第一窗口的同一组实际路径，不要新建另一份 case。在第二个真实 Windows Terminal/PowerShell 窗口，清除该测试进程内继承的 `DEEPSEEK_CODE_*`，进入 `$case\workspace`，执行：

```powershell
$env:CONPTY_FIXTURE_KEY = 'local-fixture-only'
& "$repo\target\debug\dcl.exe" --data-dir "$case\data" --base-url http://127.0.0.1:18765/v1 --api-format openai --api-key-env CONPTY_FIXTURE_KEY --model fixture --permission-mode bypass
```

按顺序输入以下提示，每一步等工具调用结束，截图显示工具名、TaskID、参数和结果。mock 的 TaskGet 等待为 1000 ms，冷启动时可能尚未 READY；可重复输入 `CONPTY get` 轮询同一任务（不写入、不重新启动），正常退出/停止的最终等待为 20000 ms。不要因一次等待超时就认定任务失败。

1. `CONPTY start`：ToolSearch → Bash → TaskGet。必须显示 `backend:conpty`、`CONSOLE_OK`、`SIZE:24:80`、`READY`，以及 `status:running`、`retrieval_status:timeout`。记录 TaskID、返回的 gate PID、工作目录 `shell.pid` 和 `descendant.pid`。`[Console]::IsOutputRedirected`/IsInputRedirected 必须为 false。
2. `CONPTY write`：显示 `echo:manual-input`。下一轮再次输入同提示，`received-lines` 有两条，任务仍运行。文件用 `Get-Content -Encoding UTF8` 查看。
3. `CONPTY resize`：TaskInput resize `{rows:35,cols:110}` 后写入，TaskGet 必须有 `SIZE:35:110` 和 `echo:after-resize`。截取新尺寸证据。
4. `CONPTY eof`：拒绝且包含 `No data was written`；`received-lines` 不出现 `MUST_NOT_WRITE`，再 write 仍成功。
5. 记录当前 session UUID。退出当前前端后，用同一数据目录和 `--resume <session-uuid>` 加以上相同 mock 参数重开；`CONPTY write` 应继续同一 TaskID。如该前端退出策略停止 owner，必须如实记录，不能用重启 shell 冒充跨轮延续；CI 单独覆盖持久 worker resume。恢复后先确认 owner 未变且任务仍运行，再检查输入；owner 已退出时拒绝输入符合限制。
6. 在停止前用下方示例固定三个进程句柄；若第 5 步已结束 owner，先记录该结果，再用 `CONPTY start` 创建新任务并等 READY，不能把新任务算作恢复成功。执行 `CONPTY stop`：TaskGet 最终 cancelled，输出中的 `cleanup_complete:true`、`final_drain_complete:true`；核对 gate/shell/descendant **三个原进程实例**均已退出。使用先打开的 `System.Diagnostics.Process` 实例并保存 `StartTime`，不要仅凭 PID 不存在判断被复用实例。不要手动 kill 后再宣称产品自动清理。
7. 新任务 `CONPTY start`，等 READY 并重新固定三个进程句柄后执行 `CONPTY normal`：显示 `echo:exit`、completed、exit_code 0，残留睡眠后代也被 Job Object 清理。与 TaskStop 的 cancelled 明确区分。
8. 新 session 用 `--permission-mode plan` 执行 `CONPTY denied`，Bash 应拒绝且工作目录无 `DENIED`。本 mock 的 denied 场景只调用 Bash，不在没有 TaskID 时追加 TaskGet。新的 session 不能对旧 task 写入；精确跨 session 自动断言见 CI。

进程证据可以在第三个 PowerShell 窗口这样固定；`$gateProcessId` 必须替换成该次 Bash 返回的真实 `terminal_pid`，不能照抄示例值：

```powershell
$gateProcessId = 12345 # 替换为本次实际值
$observedProcessIds = @(
    $gateProcessId,
    [int](Get-Content -LiteralPath "$case\workspace\shell.pid" -Raw -Encoding UTF8),
    [int](Get-Content -LiteralPath "$case\workspace\descendant.pid" -Raw -Encoding UTF8)
)
$trackedProcesses = foreach ($taskProcessId in $observedProcessIds) {
    $instance = [System.Diagnostics.Process]::GetProcessById($taskProcessId)
    $null = $instance.Handle # 在停止前打开并保持该实例的句柄
    [pscustomobject]@{ Instance=$instance; Id=$instance.Id; Created=$instance.StartTime.ToUniversalTime().ToString('o') }
}
$trackedProcesses | ForEach-Object { @{ pid=$_.Id; created=$_.Created } | ConvertTo-Json -Compress } |
    Set-Content -LiteralPath "$case\processes-before.jsonl" -Encoding UTF8
# 在 CLI/Desktop 中完成 CONPTY stop 或 CONPTY normal 后，再执行：
$trackedProcesses | ForEach-Object {
    $exited = $_.Instance.WaitForExit(15000)
    @{ pid=$_.Id; created=$_.Created; exited=$exited } | ConvertTo-Json -Compress |
        Add-Content -LiteralPath "$case\processes-after.jsonl" -Encoding UTF8
    if (-not $exited) { throw "原进程实例仍存活：$($_.Id)；先记录失败，不要把手工清理算作通过" }
}
```

保留 `model.jsonl`（仅隔离夹具内容）、工具调用记录、SQLite `tool_calls` 的独立 occurrence ID、终端文本、`shell-tasks/<session>/<task>/stdout.log` 和 `terminal-state.json`。JSON stdout 含原始 VT 序列是终端字节，不应在 UI 中误当可执行命令。完成后正常退出 mock；确需清理时只操作已核对 StartTime/句柄的测试 PID，不使用宽匹配杀进程。

## Desktop 人工验收（CI 不覆盖）

使用同次检出构建的 Desktop 或对应提交的候选包，记录二进制 SHA256。若本机没有构建前提，标记未验证，不安装依赖来绕过限制。用临时 Desktop 用户数据和空工作目录，在临时设置中添加 `http://127.0.0.1:18765/v1`、OpenAI 格式、fixture 模型和虚拟 key；不要改真实账号配置。

在 Desktop 会话中依次执行上面的 start/write/resize/eof/stop 提示，记录模型真实工具调用、结果展开/长输出显示、中文与 VT 控制序列的可读性、TaskGet timeout 后任务状态和停止后最终状态。截图同时包含会话与 TaskID。关闭该 Desktop owner 后，检查同一批已固定身份的进程是否退出；如仅关闭窗口而应用仍在托盘，分别记录窗口关闭和进程退出，不混淆 owner 生命周期。Desktop 编译通过不等于这些运行场景通过。

## 最终回传

逐项填写“Windows 已验证 / 失败 / 未验证”，附实际提交、Actions URL、每条命令与退出码、测试名/数量、精确 PID/创建时间或固定进程句柄、清理前后记录、截图/日志路径。Windows 通用 EOF、Windows pipe、跨 owner 进程输入、WSL/cmd PTY 及 owner-loss SQLite reconcile 仍是明确限制；不可把拒绝测试计成对应能力已实现。
