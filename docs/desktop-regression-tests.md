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
8. Android 用户菜单“连接服务器”：填写 `http://10.0.2.2:30185` 和令牌后能对话；登录页“在本机运行”切回后，原会话仍在，不出现“对话模型与当前模式不匹配”的提示。
9. Desktop（`npm run tauri -- dev`）仍直接进入应用，不出现登录页；在会话页刷新后不出现模型模式不匹配的提示。

## 按改动范围选择场景

- `MemoryPanel`、`api/memory`、Bridge 的 memory 路由、core 的 memory 模块：A
- `AgentCustomization`、`api/customization`、`routes/customization.rs`、`services/customization.rs`、`prompt/`：B
- `scheduler/`、`task/cron.rs`、`TaskList`、`NewTaskModal`、Desktop 入口 `main.rs` 的调度部分：C
- `CustomizePage`：B、D5
- `services/bridgeConnection.ts`、`features/server/`、`platform/native.ts`、`bridge/server_mode.rs`、`bin/dcd-server.rs`、`lib.rs` 的移动端入口、`gen/android/`：E、D1
- `features/trajectory/`（工具调用分组）：D6
- 其他范围：至少跑 D1
