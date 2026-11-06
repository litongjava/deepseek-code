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
2. 工具审批、中断、恢复会话。审批时只出现一个对话框；在浏览器和手机上点击“允许”后工具照常执行。
3. 项目和工作区切换、文件面板。
4. 模型设置和供应商设置保存。
5. Skills、MCP、插件页面可以打开和切换。
6. 让模型连续执行 3 条命令、写一句话、再执行 2 条命令：显示 `Ran commands · 3`、正文、`Ran commands · 2`，完成后都收起；执行中的分组展开并实时更新；展开后按顺序列出每次调用，点击行可看输出；深色主题同样清晰。

### E. 浏览器和手机（dcd-server）

用独立数据目录启动：`DEEPSEEK_CODE_DATA_DIR=<测试目录>/data target/debug/dcd-server --port 30185 --web-dir dist`（先 `npm run build`）。

1. 用 deepseek-browser-use 打开 `http://127.0.0.1:30185`：出现令牌登录页；错误令牌提示不正确；正确令牌进入应用，刷新后仍保持登录。
2. 发送消息，回答逐步显示；让模型调用 Bash，审批后显示结果。
3. 新建项目：填写服务器上的目录路径，信任后出现在项目列表。
4. 视口 390×844：侧边栏默认收起，打开后点选会话自动收起；会话头部不挤压标题。
5. `curl -H "Origin: https://evil.example"` 带令牌请求 `/api/conversations` 返回 403；不带令牌返回 401。
6. `--host 0.0.0.0` 不带 HTTPS 参数时拒绝启动；带自签名证书时 `https://` 可访问，登录 Cookie 带 `Secure`。用 deepseek-browser-use（Chrome 走 HTTP/2）打开 `https://<局域网 IP>:<端口>`，页面不白屏，`/assets/*` 不返回 403，登录后能对话；`curl -sk` 带同源 `Origin` 请求 `/assets/` 下的文件返回 200，带 `https://evil.example` 返回 403。`--host 0.0.0.0 --allow-http` 启动时打印明文警告；`http://<局域网 IP>:<端口>` 能登录、对话，登录 Cookie 不带 `Secure`；点击消息的复制按钮能复制；点击麦克风弹出“需要 HTTPS”说明框，不打开录音面板。
7. Android：`npx tauri android build --apk --debug --target x86_64`，在模拟器安装。首次打开直接进入应用（本机模式），添加供应商后对话；等待 Ubuntu 环境安装完成（数据目录出现 `linux/rootfs/.deepseek-ready`），让模型用 Bash 执行 `head -2 /etc/os-release` 应显示 Ubuntu，再用 `apt-get install -y git` 安装后 `git --version` 可用；通知栏显示“正在本机运行”。顶部不被状态栏遮挡，键盘弹出时输入框可见。
8. Android 用户菜单“连接服务器”：填写 `http://10.0.2.2:30185` 和令牌后能对话；从远程会话内“在本机运行”切回后进入新聊天页，原本机会话仍可从历史打开，不沿用远程会话地址，也不出现“对话模型与当前模式不匹配”的提示。换服务器同样回到新聊天页；在同一服务器重新认证时保留当前会话地址。
9. Desktop（`npm run tauri -- dev`）仍直接进入应用，不出现登录页；在会话页刷新后不出现模型模式不匹配的提示。

## Shell 配置与终端

1. 用独立数据目录启动 Desktop，通过 dsb 连接真实 WebView2；在通用设置保存 Bash 绝对路径，关闭设置后重新打开确认读回；取消文件选择不清空原路径。
2. 输入无效路径，保存失败且原配置不变。环境变量覆盖时检查页面来源提示。
3. 新建终端验证 Bash 语法、中文路径、非零退出码；原有终端保持原 Shell。
4. WSL 发行版必须手动选择；在专用测试发行版验证输入、尺寸调整、退出码和关闭终端后的 Linux 子进程清理，不使用 Docker 发行版。
5. 与 dcl 指向同一测试数据目录，确认 `config show`、模型 Bash 工具和 `!` 使用相同保存值。普通浏览器页面不能代替原生文件选择器验证。
6. Android 真机等待 Ubuntu 就绪后，从右侧面板新建终端：`head -2 /etc/os-release` 显示 Ubuntu，`pwd` 对应当前工作区；交互输入、多终端切换、旋转与键盘弹出后尺寸调整正常。连续快速输入长命令，字符顺序与输出一致；执行 `sleep 60` 后点击手机键盘栏的 `Ctrl+C` 能回到提示符，Tab 和方向键可用。关闭确认取消时保留进程，确认后清理进程；Ubuntu 尚未就绪时可使用 Android 系统 Shell。
7. 手机宽度下，会话标题和视图标签分行，模型菜单与发送按钮都在屏幕内；侧边栏可通过关闭按钮或遮罩收起，设置分类横向滚动且内容独立纵向滚动。右侧终端、文件与预览面板覆盖聊天区的可用宽度，隐藏后原会话保留。返回桌面宽度后保留原横向布局。

真实 PTY 自动化：`cargo test --locked -p deepseek-code-core --lib configured_bash_and_wsl -- --ignored --test-threads=1`，需要 Git Bash 和已安装的 `dcl-linux-test`。

## 会话 Bash 授权与工具结果

1. Ask 模式触发普通 Bash 审批：只显示一份弹窗，提供 `Allow for Session`；选择后，同一会话下一条普通 Bash 不再弹窗。
2. 用无害的 `eval 'echo test'` 命中危险规则：仍弹出单次允许/拒绝，不能选择会话授权。
3. 权限菜单显示当前会话 Bash 授权状态；点击撤销后读回未授权。其他会话和应用冷启动不继承授权。
4. 审批接口失败时保留弹窗并显示错误，不提前关闭，也不误写入会话授权。
5. `git_add` 完成后展开工具详情，结果区显示成功摘要和完整路径文本，复制输出也为文本；原始工具输入仍可检查。
6. 创建带名称的后台任务，等待 `TaskOutput` 时已显示 `TaskOutput(任务名称)`，完成后和重新打开历史后保持名称。

2026-11-06 使用隔离数据目录、实际 Tauri WebView2 和本地模型 fixture 完成 Shell 路径保存/无效路径拒绝，以及上述 1、2、3、5、6 的运行中展示。文件选择器的系统对话框、历史重开和跨平台界面尚未在本轮手工验收；对应组件/回放测试不能代替这些系统界面检查。

## 按改动范围选择场景

- `MemoryPanel`、`api/memory`、Bridge 的 memory 路由、core 的 memory 模块：A
- `AgentCustomization`、`api/customization`、`routes/customization.rs`、`services/customization.rs`、`prompt/`：B
- `scheduler/`、`task/cron.rs`、`TaskList`、`NewTaskModal`、Desktop 入口 `main.rs` 的调度部分：C
- `CustomizePage`：B、D5
- `services/bridgeConnection.ts`、`features/server/`、`platform/native.ts`、`bridge/server_mode.rs`、`bin/dcd-server.rs`、`lib.rs` 的移动端入口、`gen/android/`：E、D1
- `features/trajectory/`（工具调用分组）：D6
- 其他范围：至少跑 D1
