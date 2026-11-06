# dcd 回归测试清单

修改 `src/`、`src-tauri/` 后，agent 必须读取本文，确认改动没有影响其他功能。改到共用的 `crates/core` 时，同时执行 `docs/cli-regression-tests.md`。

## 必跑命令

```bash
git diff --check
npx tsc --noEmit
npm run test:run
cargo check --locked -p deepseek-code-desktop --all-targets
```

## 启动真实应用

没有显示器的 Linux 上，用 Xvnc 提供虚拟显示，再用独立的主目录启动，不影响真实数据：

```bash
Xvnc :19 -geometry 1400x900 -depth 24 -SecurityTypes None -localhost &
export DISPLAY=:19 HOME=<测试目录>/home RUSTUP_HOME=<原 rustup 目录> CARGO_HOME=<原 cargo 目录>
npm run tauri -- dev
```

就绪标志是 `http://127.0.0.1:30085/api/system-status` 返回 JSON。之后可以用 deepseek-browser-use 打开 `http://127.0.0.1:5200` 操作页面，开发模式的请求由同源代理转发到 Bridge。依赖 Tauri `invoke` 的功能（设置页的定时任务、原生文件操作）只能在桌面窗口验证，详见 `deepseek-code-docs` 的“浏览器调试”。

## 场景

### A. 长期记忆

1. 侧边栏打开长期记忆：统计、列表和搜索正常。
2. 添加一条，列表出现。
3. 编辑内容和重要性，保存后显示新内容；取消不改变内容。
4. 删除时出现确认；取消不删除，确认后消失。
5. `dcl memory list` 看到同样的修改（两端共用数据目录时）。
6. 导入预览未通过时不能确认导入；导出得到 JSON。

### B. 提示词与工具（自定义页）

1. 打开页面：内置工具列表完整，“保存”不可用，直到有修改。
2. 切换为“完全替换”：编辑框从内置内容开始；清空后不能保存；“恢复为内置内容”可用。
3. 关闭一个内置工具并保存，重新打开页面仍是关闭状态；发消息后模型看不到它。
4. 修改工具说明显示“已修改”，“恢复默认说明”后标记消失。
5. 添加自定义工具：Schema 不是 JSON 时报错；与内置工具重名时，保存失败并显示原因。
6. 让模型调用自定义工具：`ask` 模式出现审批，允许后结果来自命令的标准输出，命令失败时错误交给模型。
7. 替换后的提示词生效；恢复内置后回到默认行为。
8. `customization.json` 损坏时，发消息明确报错。

### C. 定时任务（设置页，需要桌面窗口）

1. 创建每分钟运行的 Send Prompt 任务：下一次时间按本地时区显示。
2. 一分钟内任务运行，记录里是模型的真实回答，不是占位文字。
3. 没有可用模型时显示失败和原因，不显示成功。
4. Webhook 和 System Command 按时真实执行。
5. 关闭应用错过时间后重新打开：补运行一次，下一次时间回到时间表。
6. 禁用的任务不运行；“立即执行”的结果与定时运行一致。

### D. 基础流程

1. 启动、新建会话、发送消息、流式显示回答。
2. 工具审批、中断、恢复会话。
3. 项目和工作区切换、文件面板。
4. 模型设置和供应商设置保存。
5. Skills、MCP、插件页面可以打开和切换。

## Shell 配置与终端

1. 用独立数据目录启动 Desktop，通过 dsb 连接真实 WebView2；在通用设置保存 Bash 绝对路径，关闭设置后重新打开确认读回；取消文件选择不清空原路径。
2. 输入无效路径，保存失败且原配置不变。环境变量覆盖时检查页面来源提示。
3. 新建终端验证 Bash 语法、中文路径、非零退出码；原有终端保持原 Shell。
4. WSL 发行版必须手动选择；在专用测试发行版验证输入、尺寸调整、退出码和关闭终端后的 Linux 子进程清理，不使用 Docker 发行版。
5. 与 dcl 指向同一测试数据目录，确认 `config show`、模型 Bash 工具和 `!` 使用相同保存值。普通浏览器页面不能代替原生文件选择器验证。

真实 PTY 自动化：`cargo test --locked -p deepseek-code-core --lib configured_bash_and_wsl -- --ignored --test-threads=1`，需要 Git Bash 和已安装的 `dcl-linux-test`。

## 按改动范围选择场景

- `MemoryPanel`、`api/memory`、`routes/memory.rs`、`crates/core/src/memory`：A
- `AgentCustomization`、`api/customization`、`routes/customization.rs`、`services/customization.rs`、`prompt/`：B
- `scheduler/`、`task/cron.rs`、`TaskList`、`NewTaskModal`、`src-tauri/src/main.rs` 的调度部分：C
- `CustomizePage`：B、D5
- 其他范围：至少跑 D1
