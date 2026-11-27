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

1. Copy a working `config.json` into the data directory. Use a local or mock
   provider; never a real key.
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
python scripts/drive_dcl.py \
  --bin target/debug/dcl.exe \
  --cwd . \
  --data-dir %TEMP%\dcl-pty \
  --config %USERPROFILE%\.deepseek-code\config.json \
  --outdir %TEMP%\dcl-shots \
  --steps "/skills" "/plugins" "/tools" "/tools" --keys Right
```

`--keys` inserts named keys between steps (`Right`, `Left`, `Space`, `Tab`),
which is how tab switching and toggles are exercised.

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
