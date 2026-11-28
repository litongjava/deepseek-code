# Driving dcl through a PTY

`dcl` only starts the REPL when `stdin` and `stderr` are terminals. Piping a
command list into it fails with `repl requires interactive stdin; use query or
--print for scripts`. A pseudo-terminal is therefore required for any test of
the prompt, slash commands, panels, or key handling.

## Dependencies

Windows, CPython 3.9+:

```bash
pip install pywinpty pyte
```

- `pywinpty` creates the ConPTY and forwards bytes.
- `pyte` renders the escape stream so assertions read a screen, not ANSI noise.

Neither is part of the app; a machine without them can still run every
non-interactive layer.

## Bootstrap a throwaway data directory

A fresh `--data-dir` first shows the provider onboarding wizard and then the
folder trust prompt, so automation must pre-seed both:

1. Put a mock-only provider config at `<data>/config/config.json`. Point it at a running loopback endpoint and use a dummy key, never a production config.
2. Write `startup-prompts.json` next to it:

```json
{
  "trusted_folders": ["\\\\?\\D:\\path\\to\\the\\workspace"],
  "bypass_accepted": true,
  "full_access_accepted": true
}
```

`trusted_folders` compares the canonicalized workspace path, which on Windows
carries the `\\?\` prefix. Pre-seeding only `bypass_accepted` is not enough:
the trust prompt still appears unless the folder is listed or bypass is on.

## Win32 input mode

ConPTY runs the app in Win32 input mode, so raw `\r`, `\x1b` or `\t` bytes do
not map to the keys the editor expects. `\r` inserts a newline and a bare ESC
is swallowed. Send key events as:

```text
CSI Vk ; Sc ; Uc ; Kd ; Cs ; Rc _
```

| Key | Down sequence |
| --- | --- |
| Enter | `\x1b[13;28;13;1;0;1_` |
| Esc | `\x1b[27;1;27;1;0;1_` |
| Space | `\x1b[32;57;32;1;0;1_` |
| Tab | `\x1b[9;15;9;1;0;1_` |
| Right | `\x1b[39;77;0;1;0;1_` |
| Left | `\x1b[37;75;0;1;0;1_` |

Repeat with `Kd=0` for the key-up event; sending both is what the editor sees
from a real terminal.

## Recipe

```python
import pyte, winpty

p = winpty.PtyProcess.spawn([r"D:\repo\target\debug\dcl.exe"], cwd=repo,
                            dimensions=(42, 150))
# read p.read() on a thread into a buffer, then:
# wait until "Type / to browse" appears before typing
p.write("/skills")
p.write("\x1b[13;28;13;1;0;1_\x1b[13;28;13;0;0;1_")
# wait ~1.5 s of quiet output
screen = pyte.HistoryScreen(150, 42, history=4000)
pyte.Stream(screen).feed(buffer)
lines = [...history lines..., *screen.display]
```

`scripts/drive_dcl.py` packages this: it bootstraps the data directory, waits
for the prompt, runs each step, captures the rendered screen, closes with Esc,
and exits the REPL.

```bash
python "$SKILL_DIR/scripts/drive_dcl.py" \
  --bin "$PWD/target/debug/dcl.exe" --cwd . \
  --data-dir "$TEST_DATA" --config "$MOCK_CONFIG" --outdir "$SHOTS" \
  --steps '/skills' '@Esc' '/plugins' '@Esc' '/tools' '@Right'
```

Set `TEST_DATA` and `SHOTS` to disposable directories and `MOCK_CONFIG` to a mock-only JSON file. Use `@Right`, `@Left`, `@Space`, `@Tab`, `@Enter` or `@Esc` as separate steps; there is no `--keys` option.

## 验收 `/config` 四个记忆开关（Windows ConPTY）

1. 在隔离数据目录启动目标 exe，指定 `--record-requests`；仅在明确要求且环境合适时加 `--yolo`。用本机 mock 接收请求；模型能力声明（包括 `supports_web_search`）应与 mock 实现匹配。用户指定默认数据目录时，先记下原开关值，结束后恢复，不要用 mock 配置覆盖个人配置。
2. 在真实终端或 ConPTY 打开 `/config`，记录 Global、Project、Session、Team 四个值。全新配置的默认值依次是开、开、开、关。每次只切一个，用同一数据目录重新启动**新会话**，重开面板确认该项已保存、另三项未变。
3. 发送唯一的测试短句，用 session ID 和短句定位主对话请求（见 `SKILL.md` §5）。检查系统提示词：打开的 scope 有对应的启用说明，关闭的没有；共有说明仍在。若要证明请求确实抵达 mock，另存端点收到的 body，与对应 `.request.json` 对比。
4. 首轮用户提醒只比较实际适用的块：Global → Team（已绑定团队）→ Project → 日期；Session 首轮可告知路径，不应注入索引正文。验证 Session 正文请先写入有辨识度的条目，触发实际压缩，检查压缩后首次重放的摘要或原生压缩项之前是否有该条目。普通新会话不能替代压缩测试。
5. 分别报告面板值、重启持久化、请求文件名和检查结果。验证跨会话记忆时，在 A 写入无敏感信息的唯一条目，在同项目、同数据目录的新会话 B 检查索引行；让 B 读取链接正文，别把只看见标题当作读到了正文。

## Traps

- Wait for `Type / to browse`; typing during startup drops input or answers
  the trust picker.
- `Esc` closes a panel; send the Win32 sequence, not `\x1b`.
- The TUI draws to `stderr`; merge both streams in the PTY reader.
- Panels may scroll into `HistoryScreen.history.top` rather than stay in
  `screen.display`; take the tail of history plus the current screen.
- Kill the process (`terminate()`) if `/exit` does not end it, or the next test
  contends for the same session locks.
- A toggle test must inspect the data directory: tool switches land in
  `<data>/config/customization.json`, skill states in `<data>/config/skill-states.json`,
  plugin and MCP state elsewhere. Do not infer a write from the panel's check
  mark alone.
