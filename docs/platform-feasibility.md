# dcd 跨平台可行性评估

评估日期：2026-11-05。结论基于本仓库代码统计和本机实测。工期是单人估算，用来比较难度，不是承诺。

## 结论

- **浏览器 + 桌面同时运行：可行，难度中等，约 2 到 4 周。** 页面已经能在浏览器运行，Bridge 不依赖 Tauri，主要工作是替换原生调用和增加登录。
- **Android：可行。** dcl 引擎已在 Android 14 模拟器上实测通过，dcd 的 Rust 代码已能通过 Android 编译检查。完整 App 难度中高，约 4 到 8 周。
- **iOS：只建议做远程客户端，约 2 到 3 周。** App Store 应用不能启动子进程，本地跑不了 Bash 这类工具。本机没有 Mac，这部分未实测。

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
