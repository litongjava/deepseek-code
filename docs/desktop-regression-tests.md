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

### A. 记忆

1. 侧边栏打开记忆（Memory）：没有会话或可信项目时提示选择范围；选择后列出当前范围的记忆文件和搜索框。
2. 新建一条记忆，列表出现；编辑后内容更新，版本冲突时提示刷新。
3. 删除前出现确认；取消不删除，确认后消失。
4. 打开“Legacy archive / remote settings”后，旧归档的统计、列表、搜索、编辑和导入导出正常。
5. `dcl memory files --project ID list` 与面板显示一致（同一数据目录时）。
6. 在归档的“记忆设置”里取消“启用长期记忆”，或在终端运行 `dcl config memory off`，两者作用于同一开关：新请求不再包含 `<memory_guidance>` 和 MEMORY.md 索引，memory 目录写入被拒绝；重新开启后恢复。

### B. 提示词与工具（自定义页的两个入口）

待批工具的审批框可用 **Cancel turn** 停止整个回合。用先用 `ToolLoad({"names":["Write"]})` 再调用 `Write` 的本地 mock 验证：
取消后审批框和运行状态结束、目标文件不创建、没有新的模型请求；失败时保留审批框并显示错误。
重启后打开已加载工具的同一会话，直接调用该工具；从真实请求确认加载资格恢复，随后双关该工具并再次强制调用，必须拒绝。
保存成功和保存错误提示都不能随切页进入另一入口，未保存草稿继续保留。
异步反例：在工具页/提示词页保存后保持请求未返回，切到另一页，再分别返回成功/失败；
两种反馈都不能出现在当前页。切走再返回原页也不能复活旧请求反馈；成功保存仍更新负责的配置半区，另一页草稿不丢失。
组件回归在 `AgentCustomization.test.tsx` 使用可控 Promise 覆盖上述 8 种组合，不能用“请求先结束再切页”代替。

1. 自定义页显示六个入口：Skills、MCP、Connectors、Plugins、提示词、工具；概览卡片顺序一致，点击进入对应页面。
2. 提示词页：切换为“完全替换”时编辑框从内置内容开始；清空后不能保存；“恢复为内置内容”可用；页面不显示工具列表。
3. 工具页：内置工具列表完整，“保存”不可用，直到有修改；页面不显示提示词设置。
4. 关闭一个内置工具的“启用”并保存，重新打开页面仍是关闭状态；只关“启用”时模型仍可通过 ToolLoad 加载它。
5. 修改工具说明显示“已修改”，“恢复默认说明”后标记消失。
6. 每个内置工具有“启用”和“按需加载”两个独立开关，改动一个不影响另一个；核心工具显示“默认核心”，但开关可以修改。
7. 只开“按需加载”的非核心工具默认不出现在请求里，模型用 ToolLoad 加载后从下一次请求起可调用；两个都关的工具不能加载或调用。
8. 关闭 `ToolLoad` 的“启用”后出现建议提示，页面不会自动重新打开它。
9. 旧配置：只在 `disabled_tools` 中关闭的内置工具显示为“启用关、按需加载开”；**只在工具页保存**后文件写入 `tool_loading` 并移除该内置名称。只保存提示词时文件里的旧条目不变；非内置条目始终保留。
10. 热变更：模型拿到常驻工具后，在一轮回答进行中把它改为“仅按需加载”，该轮内的后续调用要求先加载；改为两个都关则立即拒绝。
11. 恢复：重开会话后重新发起请求，之前成功加载过的工具仍在请求里；被重复使用或已被消费的调用 ID、出错的结果都不应让工具恢复；被改成两个都关的工具不再出现。
12. 添加自定义工具：Schema 不是 JSON 时报错；与内置工具重名时，保存失败并显示原因。
13. 让模型调用自定义工具：`ask` 模式出现审批，允许后结果来自命令的标准输出，命令失败时错误交给模型。
14. 替换后的提示词生效；恢复内置后回到默认行为。
15. 界面语言切换为英文后，入口名称、按钮和表单标签同步变为英文。
16. `customization.json` 损坏时，发消息明确报错。
17. 跨页隔离（两个入口共用一个组件实例，必须连续切换验证）：
   - 提示词页改内容不保存 → 切到工具页关一个工具并保存 → 文件里的 `system_prompt` 仍是原值。
   - 工具页关一个工具不保存 → 切到提示词页保存 → 文件里没有 `tool_loading` 的新条目。
   - 提示词页切到“完全替换”并清空 → 提示词页“保存”不可用，工具页仍可保存。
   - 回到未保存那一页，草稿仍在；保存提示只出现在执行保存的页面。
18. 离开自定义页后重新进入，并关闭测试 Desktop 进程后重新启动：提示词和工具读回各自已保存的值；未保存的工具改动不应落盘。草稿保留仅指提示词与工具入口之间连续切换，不保证离开自定义页或重启后保留。
19. 按需加载后的权限：先加载一个需要审批的工具，拒绝审批时工具不执行且无副作用；审批等待期间取消本轮不会继续请求。
20. 以上 1–19 项须用**本次构建**的 Desktop 与 Bridge 在真实 WebView2 中执行。旧单开关界面的历史验证结果只作为历史记录保留，不能替代本项。
21. Skills 入口的示例技能 skill-creator 显示后端返回的文件树：`SKILL.md`、`scripts/`、`references/`、`license.txt`、`NOTICE.md`；展开 `scripts` 有 `init_skill.py` 和 `quick_validate.py`，打开文件能看到内容。不得出现旧占位或硬编码的 `agents/`、`eval-viewer/` 等不存在的文件。旧数据目录中未修改的占位会被移除，改动过的副本原样保留。
22. 在工作明细和轨迹打开一次 `ToolLoad` 调用。验证其显示为 `ToolLoad(Read, Grep)`，名称较多时显示省略号。对包含 CJK、emoji 或组合字符的被拒绝名称，检查长名称仅在字素边界截断，窄窗中保留右括号并可换行。调用原始参数和结果可查看。组件覆盖见 `trajectory.test.tsx`。

2026-11-07 在 Windows 的真实 Tauri WebView2 中完成 B11、B12（**当时编号**：单开关工具页时代的自定义工具与提示词项，不等同于本清单现在的编号，也不构成双开关页面与加载策略的验收证据）。使用现有 `target/debug/dcd.exe`、当前 Vite 前端源码和独立数据目录，通过 dsb 鼠标点击保存、原生选择框及输入控件操作；同时核对真实后端生成的 `customization.json`。两个方向的隔离、空替换提示词不阻塞工具保存、草稿保留、提示隔离、页面重开和进程重启读回均通过。相关两个组件测试文件共 16 项通过，`npx tsc --noEmit` 通过。本轮未重新构建或替换可执行文件，也未验证模型调用或其他平台。详细记录与 dsb 脚本原位于 `dev-support/deepseek-code/customize-native-20261107/`，已在 2026-11-10 的开发支持目录清理中删除，本轮结论只保留在此处。

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
7. 排队后续消息：回合进行中排队一条消息，回合结束后该消息自动开始执行，队列未进入暂停状态（不出现 409 或“Queued turn failed admission”）。

### D2. 请求失败诊断（共享 core 路径）

使用本机故障服务配一个独立数据目录，不使用真实密钥。原 `api-failure-audit-20261107/desktop/fixture.py` 已在 2026-11-10 的开发支持目录清理中删除，执行本节前需参考 `crates/cli/tests/api_failure_diagnostics.rs` 与 `crates/core/tests/api_failure_*_probe.rs` 中的同类本地 HTTP 场景重建服务。

1. 连接在发送阶段被断开：错误卡片显示失败阶段、类别与实际尝试次数，服务端收到的请求数与之相同；URL 只保留协议、主机和端口。
2. 服务端返回 401 并在正文中回显 `Bearer`、多项 `Cookie`、`Basic` 凭据：卡片只显示认证失败与状态码，页面全文不含这些值，且只发出 1 次请求。
3. Desktop 自身数据库中的请求快照阶段、状态码与次数和卡片一致。
4. 已输出部分正文后断流：已显示内容只出现一次，不重复写入历史；卡片显示流式中断、HTTP 200 与实际尝试次数。
5. 服务端把用户提问回显到 `error.message`：卡片只显示类别与已知错误标识，页面全文不含提问内容。

### D3. 共享压缩逻辑（与 CLI 相同的 core 路径）

Desktop 的自动压缩、手动压缩和局部摘要都调用 `crates/core` 的同一套摘要实现，不需要单独一套断言。改动 `native_engine/generation.rs`、`services/compaction*` 时，先按 `cli-regression-tests.md` 的 L 节跑完整回归，再在桌面窗口确认：

1. 超长会话在轮次开始前自动压缩，界面的进度行依次显示分段、合并层和最终摘要；压缩完成后原请求只继续一次，回答基于摘要而不是整段原文。
2. 手动压缩（输入框上方的压缩入口）显示同一套进度；压缩完成后历史记录仍可回看。
3. 摘要失败时不写入压缩边界：会话保持原样，界面显示失败阶段、预算和“原始历史保留”，重新发送不会重复执行已完成的工具调用。
4. 恢复期间取消（停止当前回复）后历史不变；再次压缩时已完成的分段直接复用，进度显示复用计数。
5. 在隔离数据目录里用本地 mock 保存带唯一值的 Read 结果，再生成省略该值的摘要并继续。新会话请求的工具目录可发现 `ContextSearch`。同一会话压缩后，即使 Desktop 发出的请求没有边界元数据，应用已保存边界后也会提示通过 `ToolLoad` 加载。核对该值首次出现在检索结果里，引用的消息和工具调用 ID 与压缩前的 SQLite 记录一致。关闭两个工具开关后复验提示和调用均不可用。

### D4. 技能目录（共享 core 路径）

Desktop 的模型技能列表和 `Skill` 工具与 CLI 共用 `crates/core` 的发现入口，改动 `skills/discovery.rs`、`native_engine/engine_core.rs` 的技能列表或 `native_engine/skill_tool.rs` 时，除 `docs/cli-regression-tests.md` 的 N 节外，在桌面窗口确认：

1. 用独立数据目录启动 Desktop，只在 `<数据目录>/skills/<名称>/SKILL.md` 放一个技能，发送普通请求：模型的 `Available skills` 含该名称。
2. 让模型调用 `Skill`：工具成功，说明中的技能目录是数据目录下的真实路径，引用 `references/...` 可读取。
3. 同名技能放在工作区 `.agents/skills`、`.claude/skills` 和数据目录：按 `.agents` > `.claude` > 数据目录选择，模型列表只出现一项，技能面板/斜杠名称一致；`~/.claude` 不参与发现。
4. `enabled: false` 与 `disable-model-invocation` 的显示和调用行为与 CLI 一致。
5. `<数据目录>/skills/bundled`、`mcp` 中的同名副本不覆盖内置工作流。

### D5. 会话分库与 CLI 导出

1. 用独立数据目录启动 Desktop，创建两个会话，各发送不同的消息。核对侧边栏标题、项目和归档状态与 `global.db` 的已发布目录一致，分别打开会话时，历史来自对应的 `sessions/<storage_key>/session.db`。
2. 对同一数据目录运行 `dcl --data-dir <目录> sessions export <id> --output <file>`。逐行解析 JSONL，核对头部会话 ID、incarnation、消息顺序、审批、附件路径和轨迹中的请求。归档后重复导出。流式回答期间导出时，核对已提交片段和运行状态；随后收到的片段应出现在下一次导出。
3. 运行 `cargo test --locked -p deepseek-code-desktop --test conversation_generation_bridge --test input_queue_bridge --test trajectory_persistence` 覆盖 Bridge 的生成、排队和轨迹。运行 `cargo test --locked -p deepseek-code-cli --test session_export` 校验导出末行计数、SHA-256、文件名冲突和旧库三个文件的哨兵。

### E. 浏览器和手机（dcd-server）

用独立数据目录启动：`DEEPSEEK_CODE_DATA_DIR=<测试目录>/data target/debug/dcd-server --port 30185 --web-dir dist`（先 `npm run build`）。

1. 用 deepseek-browser-use 打开 `http://127.0.0.1:30185`：出现令牌登录页；错误令牌提示不正确；正确令牌进入应用，刷新后仍保持登录。
2. 发送消息，回答逐步显示；让模型调用 Bash，审批后显示结果。
3. 新建项目：点击“浏览文件夹”，在选择框中逐级进入服务器上的目录（也可输入路径后点“打开”），点“使用此文件夹”后路径出现在列表中；选择框显示在创建对话框之上，在选择框内按 Esc 只关闭选择框。信任后出现在项目列表。在侧边栏编辑项目时同样添加文件夹，编辑面板不被关闭。
4. 视口 390×844：侧边栏默认收起，打开后点选会话自动收起；会话头部不挤压标题。
5. `curl -H "Origin: https://evil.example"` 带令牌请求 `/api/conversations` 返回 403；不带令牌返回 401。
6. `--host 0.0.0.0` 不带 HTTPS 参数时拒绝启动；带自签名证书时 `https://` 可访问，登录 Cookie 带 `Secure`。用 deepseek-browser-use（Chrome 走 HTTP/2）打开 `https://<局域网 IP>:<端口>`，页面不白屏，`/assets/*` 不返回 403，登录后能对话；`curl -sk` 带同源 `Origin` 请求 `/assets/` 下的文件返回 200，带 `https://evil.example` 返回 403。`--host 0.0.0.0 --allow-http` 启动时打印明文警告；`http://<局域网 IP>:<端口>` 能登录、对话，登录 Cookie 不带 `Secure`；点击消息的复制按钮能复制；点击麦克风弹出“需要 HTTPS”说明框，不打开录音面板。
7. Android：`npx tauri android build --apk --debug --target x86_64`，在模拟器安装。首次打开直接进入应用（本机模式），添加供应商后对话；等待 Ubuntu 环境安装完成（数据目录出现 `linux/rootfs/.deepseek-ready`），让模型用 Bash 执行 `head -2 /etc/os-release` 应显示 Ubuntu，再用 `apt-get install -y git` 安装后 `git --version` 可用；通知栏显示“正在本机运行”。顶部不被状态栏遮挡，键盘弹出时输入框可见。
8. Android 用户菜单“连接服务器”：填写 `http://10.0.2.2:30185` 和令牌后能对话；从远程会话内“在本机运行”切回后进入新聊天页，原本机会话仍可从历史打开，不沿用远程会话地址，也不出现“对话模型与当前模式不匹配”的提示。换服务器同样回到新聊天页；在同一服务器重新认证时保留当前会话地址。
9. Desktop（`npm run tauri -- dev`）仍直接进入应用，不出现登录页；在会话页刷新后不出现模型模式不匹配的提示。
10. 直接运行构建产物（不经过 Cargo，环境中没有 `RUST_MIN_STACK`）：`env -u RUST_MIN_STACK target/debug/dcd-server ...`，让模型调用 Bash 工具。服务器不崩溃，回答正常显示。
11. 请求数量（弱网）：在页面中用 `performance.getEntriesByType('resource')` 统计。首页加载后空闲 60 秒内没有新请求；会话页空闲 60 秒内 `queue` 和 `pending-questions` 各不超过 3 次，会话详情只请求 1 次；页面切到后台后停止轮询。`curl -H 'Accept-Encoding: gzip'` 请求 `/assets/` 下的 JS，返回 `Content-Encoding: gzip` 和 `Cache-Control: public, max-age=31536000, immutable`；首页返回 `no-cache`。没有对 `fonts.googleapis.com` 的请求。
12. 断网提示：dsb `set_offline` 断网后发送消息，底部提示“无法连接服务器……”（警告图标），输入框保留原文，上一轮回答不变；恢复网络后点发送，正常回答。运行中断网时，回答区显示同样的提示，不出现 `Failed to fetch` 或 `network error`。
13. 带 `?snapshot_parts=1` 请求会话详情，响应含 `snapshotParts`，快照中的 `systemPrompt`、`tools` 换成 `systemPromptRef`、`toolsRef`；页面的轨迹视图与不带参数时一致。

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
5. Bash 完成长输出后展开工具详情，结果区显示完整输出文本，复制输出也为文本；原始工具输入仍可检查。
6. 创建带名称的后台任务，模型用 `TaskGet`（`block:true`）等待时已显示 `TaskGet(任务名称)`，完成后和重新打开历史后保持名称。

2026-11-06 使用隔离数据目录、实际 Tauri WebView2 和本地模型 fixture 完成 Shell 路径保存/无效路径拒绝，以及上述 1、2、3、5、6 的运行中展示。文件选择器的系统对话框、历史重开和跨平台界面尚未在本轮手工验收；对应组件/回放测试不能代替这些系统界面检查。

## 按改动范围选择场景

- `MemoryPanel`、`api/memory`、Bridge 的 memory 路由、core 的 memory 模块：A
- `AgentCustomization`、`api/customization`、`routes/customization.rs`、`services/customization.rs`、`prompt/`：B
- `scheduler/`、`task/cron.rs`、`TaskList`、`NewTaskModal`、Desktop 入口 `main.rs` 的调度部分：C
- `CustomizePage`：B、D5
- `services/bridgeConnection.ts`、`features/server/`、`platform/native.ts`、`bridge/server_mode.rs`、`bin/dcd-server.rs`、`lib.rs` 的移动端入口、`gen/android/`：E、D1
- `features/project/FolderPicker.tsx`、`FolderFields.tsx`、`routes/filesystem.rs`、core 的 `fs/directories.rs`、`components/ui/Modal.tsx`：E3、D3
- 轮询与请求合并（`QueueDock`、`PlanApprovalController`、`useVisiblePolling`、`utils/inflight.ts`）、压缩与缓存（`bridge/mod.rs` 的静态文件）、`routes/conversations.rs` 的快照共享：E11、E13、D1、D2
- `utils/networkError.ts`、`utils/chatErrors.ts`、`MainContent` 的发送失败处理：E12、D1
- `lib.rs` 的运行时（`build_runtime`）、Desktop 和 dcd-server 入口：E10、D1、D2
- `crates/core` 的摘要实现（`native_engine/generation.rs`、`services/compaction*`）：D3，以及 `cli-regression-tests.md` 的 L 节
- 语音输入（`VoiceInput`、`peripheral.rs` 的 `voice_transcribe`）：Desktop 中配置音频转写后端，录音 5 秒后停止并转写，文本正确；录音以二进制传给后端（无 JSON 数字数组）。浏览器的 HTTP 页面点击麦克风弹出“需要 HTTPS”。
- `features/trajectory/`（工具调用分组）：D6
- `ToolPermissionPrompt`、`ToolCallDisplay`、`ToolDiffView`、`AssistantMessage` 的 Python 分支：D1，以及下方“Python 工具审批与展示”
- `crates/core` 的技能发现、模型技能列表和 `Skill` 工具：D4，以及 `cli-regression-tests.md` 的 N 节
- 其他范围：至少跑 D1

### ToolScript 内层审批来源

复用 `ToolPermissionPrompt` 和已有权限响应路由。模型经 ToolLoad 加载 ToolScript 后，ask 模式中的内层 Write 应展示实际输入以及 ToolScript 父调用 ID，允许/拒绝与普通工具一致；取消或 TaskStop 后提示消失，旧回复不能执行工具。`ToolPermissionPrompt.test.tsx` 覆盖来源文字与既有按钮行为；真实 Desktop/WebView 和私有 worker 的全链路需另行运行，不以 SDK 测试代替。

### 文件记忆模式补充（2026-11-13）

A 节旧流程适用于显式 Legacy archive 入口；默认 Project memories 面板改为当前会话/可信项目的 Markdown 记忆：

1. 无会话/项目时不查询全局记忆；选择项目后显示文件 name、description、type。与同 data_dir、同 Git common-dir 的 `dcl memory files list` 对照。
2. 新增带 frontmatter 的文件；编辑正文保留 metadata，feedback/project 缺 Why 或 How to apply 应报错。删除取消不变，确认后索引同步删除。
3. 两个窗口读同版本，窗口一保存后窗口二保存报 409；第二窗口草稿保留，刷新后重读。切换 scope 时旧请求返回不能显示前一项目内容。
4. 缺 scope 的旧 `/api/memory/ingest`、缺版本更新/删除明确失败；Legacy archive 下的导出/导入继续保留旧数据，默认项目召回不使用它们。
5. 在临时 data-dir 中为各 scope 的 `MEMORY.md` 和正文写不同标记。对照 Desktop 实际模型请求：首条用户消息在项目指令之后依次是 global、可选 team、project、仅子代理的 subagent、当前 session；各有路径，无自动附加的正文，也无父 session 泄漏。编辑两份索引后，下次请求仅新增一条合并提醒，冻结首条不变；重复发送不新增提醒。手动 compact 后索引各一次，无边界前提醒；关闭重开会话比较相同历史前缀，再改一个索引确认仅它产生新提醒。构建检查不等于此 WebView 场景通过。
6. Plan Read 成功，Write/Edit/ApplyPatch 删除拒绝；Router/Coder 门禁、显式 deny/ask 与 hooks 不被目录免确认绕过。禁用记忆后不注入，配置远程 HTTP 后只有远程工具可加载；本地工具目录不得残留 memory_search/ingest。
7. 自动化组件覆盖文件编辑、CAS 参数、冲突草稿、删除取消和显式旧归档入口；这不代表原生 Desktop/WebView2、Windows reparse 和服务器/手机操作已验证。

## Python 工具审批与展示

自动检查：

```bash
npx tsc --noEmit
npm run test:run -- src/components/ToolPermissionPrompt.test.tsx
```

在真实 Desktop/WebView 中：

1. 用 `ToolLoad` 加载 `Python`，在 ask 模式触发调用。审批框标题为 “Python code”，显示完整代码、描述、`env` 的 `KEY=value`、`cwd`、`interpreter` 或 `conda`，以及 `Will write: <当前项目工作区>/temp/<save_name>` 的绝对路径；不显示 “Allow for Session”。
2. 选择 Allow 后，工具调用摘要显示 “Run Python” 和 `description | 首行代码或保存路径`；展开详情能看到代码、运行参数和输出。`ToolDiffView` 的 Python 视图显示代码块、运行参数和 stdout/stderr。
3. 选择 **Cancel turn** 终止整个回合；选择 **Deny** 只拒绝当前调用，模型继续回答。审批接口报错时弹窗保留并显示原因。
4. 计划模式发起 Python 调用被拒绝；切出 Plan 模式后可正常审批。
5. 开启沙箱（Linux/macOS）后 Python 仍被包装；Windows 上显示明确的“不支持沙箱”错误，不静默在沙箱外执行。
6. `save_name` 的目标已存在时审批仍显示同一路径；批准后文件被覆盖，结果为 `script_saved:true` 且 `script_path` 是绝对路径。目标为符号链接时工具报错，不写入。
7. 后台 Python 调用完成后，`TaskGet` 的结果和 `/tasks` 中的任务名使用描述或保存名，不包含整段代码。

### Experience 索引（2026-11-25）

1. 在项目工作区根与 `docs/.agents/experience/` 各放一个条目，新建会话；首条用户消息在 MEMORY.md 索引之后、前端说明之前出现 Experience 块，索引行是 `- 相对路径 | 名称: 描述`，不含正文，也不含 `docs/sub/.agents/experience/` 的三级条目。
2. 同一会话让模型新增一个条目：下一轮只出现一次 `New experience:`，首条消息不变；再次请求不重复。
3. 手动压缩上下文：Experience 块重新附在剩余的第一条用户消息上，只出现一次。
4. 无项目会话（内部草稿工作区）不出现 Experience 块。
5. 在 `/config` 关闭 Experience 后新建会话不出现；重新打开后新会话恢复。
6. 删除一个条目：下一轮出现一次 `Removed experience: <name>`。

本机（macOS）只执行了 core/CLI 自动化；Desktop WebView、Windows 与真实 Tauri 窗口的上述步骤尚未在本轮验证。
