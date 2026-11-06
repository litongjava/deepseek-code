# dcd 跨平台可行性评估

评估日期：2026-11-05。结论基于本仓库代码统计和本机实测。工期是单人估算，用来比较难度，不是承诺。

## 结论

- **浏览器 + 桌面同时运行：可行，难度中等，约 2 到 4 周。** 页面已经能在浏览器运行，Bridge 不依赖 Tauri，主要工作是替换原生调用和增加登录。
- **Android：可行。** dcl 引擎已在 Android 14 模拟器上实测通过，dcd 的 Rust 代码已能通过 Android 编译检查。完整 App 难度中高，约 4 到 8 周。
- **iOS：只建议做远程客户端，约 2 到 3 周。** App Store 应用不能启动子进程，本地跑不了 Bash 这类工具。本机没有 Mac，这部分未实测。

## 改造进展（2026-11-05）

浏览器 + 桌面的第 1 到 4 步已完成；Android 第 2、3 阶段已完成（远程模式、进程内引擎、PRoot + Ubuntu、前台服务、APK 构建），发布签名与分发渠道需要维护者的签名密钥；iOS 可用手机浏览器添加到主屏幕，或用同一移动端入口构建（未在 iOS 上验证）。用户说明见文档站“在浏览器和手机上使用”。

- **服务程序**：`dcd-server` 运行 Bridge 并托管 `dist`，单页应用的路由回退到 `index.html`。默认只监听本机且不换端口；监听网络地址时必须 `--tls-cert/--tls-key` 或 `--behind-https-proxy`，否则拒绝启动。
- **认证**：访问令牌保存在数据目录的 `server-token`（0600）。接口接受 `x-api-key`、Bearer 和登录后的 HttpOnly、SameSite=Strict Cookie，任一有效即通过。服务器模式下 `/metrics`、`/api/workspace-config`、`/api/system-status` 也需要令牌。规则集中在 Bridge 的 `server_mode` 模块，有单元测试和真实服务器的集成测试。
- **来源校验**：带 `Origin` 的请求必须与 `Host` 同源，或在允许列表中（默认含 Tauri 应用来源，可用 `--allow-origin` 追加）。登录接口同样校验，其他网站不能借用户浏览器猜令牌。
- **前端连接**：连接模块（`bridgeConnection`）分三种模式：桌面（本进程 Bridge）、开发代理、web（页面所在服务器或保存的服务器地址）。web 模式由 `ServerGate` 组件显示登录页；401 时回到登录页。
- **原生调用**：适配层 `invokeDesktop` 统一处理。桌面调用 Tauri；浏览器和手机对已登记的命令走 HTTP（`peripheral_command` → `/api/peripheral`，覆盖 18 处调用），其余抛出“需要桌面版”。`peripheral_command` 拆出不依赖 Tauri 的 `run()`，服务器上的服务凭据来自环境变量。
- **附件**：图片和文件用带凭据的 `fetch` 取得 blob URL，不再拼接 URL 令牌；桌面和手机应用（跨源）也因此能显示附件图片。
- **窄屏**：宽度小于 768px 时侧边栏改为抽屉，导航后自动收起；标题栏和会话头部隐藏次要按钮。加入 `manifest.webmanifest`，可添加到主屏幕。
- **取消 30090 网关**：原 Node 网关 `api-proxy` 已删除，能力并入 Bridge：GitHub 连接（OAuth 与个人访问令牌、仓库浏览、复制文件到会话工作区）在 `/api/github/*`；数据同步走 Bridge 的 `/data/sync/*`；模型代理由 Bridge 的供应商配置直接连接。可选的托管账号服务通过构建变量 `VITE_ACCOUNT_SERVICE_URL` 指定，未配置时相关调用立即报错，不再请求本机端口。
- **底层修复**：`OrchestrationConfig::get()` 首次使用时自动加载，未调用 `init_global()` 的程序（如 `dcd-server`）不再在聊天时崩溃。

- **Android 应用**：移动端入口在应用进程内启动 Bridge 和引擎，数据放在应用私有目录，页面按桌面的方式通过 `get_bridge_endpoint` 连接，默认在本机运行，不需要服务器。用户菜单的“连接服务器”切换到远程模式，登录页的“在本机运行”切回。Android 与 iOS 的配置放宽 CSP 的 `connect-src`，允许连接任意服务器。Android 工程已纳入仓库，`MainActivity` 按状态栏、导航栏、刘海和键盘设置内边距（Android 15 起强制全面屏）。跨源客户端只用 Bearer，服务器不需要允许带凭据的跨源请求。
- **Ubuntu 环境（PRoot）**：APK 以 `lib*.so` 形式在 jniLibs 中附带 Termux 构建的 PRoot、loader、talloc 和 android-shmem（`npm run android:proot` 下载并按 SHA-256 校验，再修正库名和搜索路径，许可证随 assets 附带）。首次启动时后台下载 Ubuntu 24.04.5 base（约 30 MB，校验发布方签名清单中的 SHA-256）并解压到私有目录；硬链接改为复制，设备文件跳过，写入 DNS 和 apt 免降权配置。就绪后 Bash 工具通过 PRoot（`-0 --link2symlink`，绑定数据目录和工作区，路径一致）在 Ubuntu 中执行；就绪前使用系统 `sh`。
- **前台服务**：`EngineService` 以 specialUse 类型的前台服务保持应用进程，任务在后台继续；从最近任务划掉应用时停止。Android 13 起请求通知权限，未授权时服务照常运行。
- **网络安全配置**：发布版只允许对 127.0.0.1 使用明文 HTTP（应用内引擎），远程服务器必须 HTTPS；调试版允许局域网 HTTP。
- **会话模型误报**：页面刷新后会话可能先于模型列表加载，以前会误报“对话模型与当前模式不匹配”。现在模型列表到达后重新判断，可用时自动消除提示。桌面同样受益。

实测 Android（Docker 中的 Android 14 x86_64 模拟器，调试 APK）：
- 本机模式：首次打开直接进入应用，配置供应商后真实模型对话成功。Ubuntu 环境自动安装完成，Bash 工具在其中执行 `head -2 /etc/os-release`（Ubuntu 24.04.5 LTS）；模型用 `apt-get install git python3` 安装后得到 `git version 2.43.0`、`Python 3.12.3`。前台服务处于前台状态（`isForeground=true`）。
- 远程模式：连接主机上的 `dcd-server`（`http://10.0.2.2:<端口>`），登录、对话成功。
- 两种模式通过用户菜单和登录页来回切换，切换后会话保留，不出现误报提示；界面不被状态栏和键盘遮挡。
- 测试注意：宿主机 `adb` 与容器共用 5037 端口（host 网络），构建会启动宿主机 adb 抢走设备，测试前先 `adb kill-server`；容器重启会因 `/dev/kvm` 权限失败，需要删除后重建。

没有验证：真实手机、arm64（arm64 的 PRoot 组件已打包，未在 arm64 设备上运行）、发布版签名与分发渠道、iOS（没有 Mac，iOS 只完成了相同入口和配置；App Store 应用不能启动子进程，iOS 本机模式只能对话和读写沙箱文件）。

实测（Linux，Chrome 无头，deepseek-browser-use）：令牌登录、真实模型对话、Bash 工具审批与执行、新建项目（填写服务器路径）、390×844 手机视口的布局与抽屉；`curl` 验证 HTTPS（HTTP/2、Secure Cookie）和跨源拒绝。

## 实测结果

本次验证的内容：

- dcl 交叉编译到 `x86_64-linux-android` 成功，`aarch64-linux-android` 编译检查通过。
- Android 14 x86_64 模拟器（Docker + KVM）上，通过 `adb shell` 运行 dcl：真实模型回答正确，`Bash`、`Write`、`Read`、长期记忆可用。
- Termux 中用 `apt` 安装 `git`、`python` 后，模型调用它们完成了一次真实的 Git 提交。
- 系统自带 toybox 提供 `ls`、`grep`、`sed`、`awk`、`find`、`tar`；缺 `git`、`curl`、`python3`、`node`。
- dcd 对 `x86_64-linux-android` 的 `cargo check` 通过（修复 1 处错误后）。这只说明能编译，没有打包 APK，也没有在手机上运行界面。

Android 版 dcl 目前没有这些功能，调用时会明确报错：麦克风录音、键鼠控制、工作流脚本（`rquickjs` 没有 Android 绑定，生成绑定需要可用的 libclang）。

没有验证：真实手机、arm64 上实际运行、Android 上的 PTY 交互界面长时间使用、任何 iOS 内容。

## 代码现状

- 页面调用原生能力：`invoke` 出现 156 处，分布在 33 个文件，涉及 125 个命令；`@tauri-apps` 被 36 个文件引用；Rust 侧有 157 个 Tauri 命令。
- Bridge 有 186 条 HTTP 路由，`bridge/` 目录不依赖 `tauri::`。
- 聊天、供应商、记忆、提示词与工具等主要功能已走 HTTP。

## 浏览器 + 桌面

要做的事：

1. 把 Bridge 拆成可独立运行的服务程序（如 `dcd-server`），同时托管前端静态文件。
2. 把页面里 125 个 `invoke` 命令按使用情况分类：能走 HTTP 的增加路由并统一到一个适配层，只有桌面才有意义的（托盘、窗口、在文件管理器中打开）在浏览器隐藏。
3. 替换文件选择（上传或服务器目录浏览）、剪贴板（浏览器 API）、通知等插件。
4. 增加登录和权限。**这是风险最大的一项：** 现在的 Bridge 假定只有本机使用，而浏览器版等于把运行命令的能力暴露到网络上。默认只监听本机，开放访问必须有认证、HTTPS 和来源校验。

## Android

推荐架构，与 DSH Go（已分析其公开代码）的思路一致：

- 界面：Tauri 2 的 Android WebView 复用现有页面。
- 引擎：Bridge 和引擎在 App 进程内运行。
- 命令环境：App 内置 PRoot 和 Ubuntu rootfs，免 Root、免 Termux，模型的命令在 Ubuntu 里运行，`git`、`python` 等都可以用。

关键约束：

- Android 10 起，面向新 SDK 的 App 不能执行数据目录里的文件，所以可执行文件要改名为 `lib*.so` 放进 `jniLibs`，由系统解压到可执行目录。DSH Go 正是这样打包 proot。
- 需要前台服务才能让任务在后台继续。
- rootfs 使 APK 增大数百 MB，可以首次启动时下载或解压。
- 不需要自己写解析模型命令的工具层：模型输出的已经是 shell 命令，给它一个完整的 Linux 环境比重新实现命令更可靠。

分阶段：

1. **已可用：** Termux 中运行 dcl（见用户文档）。
2. **1 到 2 周：** Tauri Android 工程打包现有页面，先连接电脑上的 Bridge（远程模式）。
3. **3 到 6 周：** 进程内引擎、PRoot + Ubuntu、前台服务、APK 分发。

许可：DSH Go 是 AGPL-3.0，只参考架构，不复制代码。proot（GPL-2.0）等组件要随 APK 附带许可证和源码获取方式。

## iOS

- App Store 应用不能 `fork`/`exec`，也不能使用 JIT，PRoot 方案走不通。
- 本地能做的只有不依赖子进程的工具（读写沙箱内文件、网络请求、模型对话），写代码场景价值有限。
- 建议做远程客户端：Tauri iOS 或 PWA 复用页面，连接电脑或服务器上的 Bridge。依赖“浏览器 + 桌面”第 1、4 步。
- 需要 Mac、Xcode 和 Apple 开发者账号，本机无法构建和测试。

## 建议顺序

先做浏览器版的服务拆分和认证，因为 Android 远程模式和 iOS 都复用这部分；再做 Android 本地引擎；iOS 最后，以远程客户端形式发布。
