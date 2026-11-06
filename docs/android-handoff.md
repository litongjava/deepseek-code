# Android 真机测试交接

日期：2026-11-05。本文给在另一台电脑上用 Android 真机测试的人。功能说明见文档站“在浏览器和手机上使用”，架构与进展见 `docs/platform-feasibility.md`。

## 当前状态

已完成并在 Android 14 x86_64 模拟器（调试版）上验证：

- 本机模式：应用进程内运行 Bridge 和引擎，首次打开直接进入应用，无需服务器。
- Ubuntu 环境：首次启动后台下载 Ubuntu 24.04 base（约 30 MB，校验 SHA-256），模型的 Bash 命令通过 PRoot 在其中执行；`apt-get install git python3` 后可正常使用。
- 前台服务：任务进行时通知栏显示“正在本机运行”，切到后台继续执行。
- 远程模式：用户菜单“连接服务器”，填写 dcd-server 地址和访问令牌；登录页“在本机运行”切回。
- 界面避开状态栏、导航栏和键盘；设置页、供应商编辑在窄屏上分行排列。

发布版（release）在模拟器上验证了：安装、启动后连上应用内引擎、前台服务运行、设置页布局。发布版的对话、Ubuntu 命令和 apt 安装没有在本机跑完：模拟器在本机内存和 CPU 不足，系统界面长时间无响应、adb 频繁断开。

从未验证：真机、arm64 设备上的 PRoot 与 Ubuntu、正式签名和分发。

## 拿到 APK

方式一：使用已构建的测试包。开发目录 `runs/android-release/` 下的 `release-test-arm64.apk` 是同时包含 arm64-v8a 和 x86_64 的发布版，用测试密钥签名（只用于测试，不能发布）。复制到测试电脑后：

```bash
adb install -r release-test-arm64.apk
```

方式二：自己构建。需要 Linux、macOS 或 WSL（PRoot 下载脚本用到 `ar` 和 Linux 版 `patchelf`，Windows 原生环境运行不了），以及 Node.js 22、Rust（带 `aarch64-linux-android` 目标）、Android SDK（platform 34+、build-tools）、NDK r27、JDK 17 或 21。设置 `ANDROID_HOME`、`NDK_HOME`、`JAVA_HOME` 后：

```bash
npm ci
npm run android:proot                                  # 下载并校验 PRoot 组件
npx tauri android build --apk --target aarch64         # 发布版，未签名
npx tauri android build --apk --debug --target aarch64 # 调试版，可用 run-as 查看数据目录
```

发布版输出在 `src-tauri/gen/android/app/build/outputs/apk/universal/release/`，签名示例：

```bash
zipalign -f -p 4 app-universal-release-unsigned.apk aligned.apk
apksigner sign --ks <你的密钥库> --out app-release.apk aligned.apk
```

排查问题时优先装调试版：可以 `adb shell run-as com.deepseek.code` 进入数据目录，`adb logcat` 能看到 Rust 日志（标签 `RustStdoutStderr`）和页面日志（标签 `Tauri/Console`）。

## 测试步骤

每一步都记录结果；失败时保存 `adb logcat -d > logcat.txt` 和截图。

1. 安装后首次打开：直接进入聊天页，没有登录页；系统询问通知权限时允许。
2. 左下角用户菜单 → 设置 → Models → 供应商右侧“+” → 填写 DeepSeek 的地址、密钥和模型，保存。返回聊天页，模型选择器显示该模型。
3. 发送“回复 OK”：流式显示回答。
4. 等待 Ubuntu 环境安装完成（首次约 1～3 分钟，取决于网速）。调试版可用 `adb shell run-as com.deepseek.code ls linux/rootfs/.deepseek-ready` 确认。
5. 让模型用 Bash 执行 `head -2 /etc/os-release`：允许后结果应为 Ubuntu 24.04。安装完成前执行，会显示 Android 自带 Shell 的结果，这是预期行为。
6. 让模型执行 `apt-get update && apt-get install -y git python3 && git --version && python3 --version`（超时设为 600000）：成功输出版本号。
7. 发送一个耗时任务（如 `sleep 60; echo done`），立刻切到其他应用：通知栏显示“正在本机运行”，1 分钟后回来，任务已完成。从最近任务划掉应用后，通知消失。
8. 连续执行多条命令，确认聊天中显示可折叠的 `Ran commands` 分组。
9. 旋转屏幕、弹出键盘：顶部不被状态栏遮挡，输入框不被键盘挡住。
10. 远程模式：电脑运行 `dcd-server --host 0.0.0.0 --port 8443 --tls-cert <证书> --tls-key <私钥>`（发布版只允许 HTTPS 远程地址；调试版可用局域网 HTTP），手机用户菜单“连接服务器”，填写地址和 `dcd-server --print-token` 的令牌，能对话；登录页“在本机运行”切回后原会话仍在。

## 可能遇到的问题

- 第 5 步一直不是 Ubuntu：检查网络能否访问 `cdimage.ubuntu.com`；调试版看 logcat 中 `LinuxEnv` 的报错。安装失败会在下次启动重试。
- 第 5 步报 PRoot 错误：记录 logcat 和设备型号、Android 版本、CPU 架构（`adb shell getprop ro.product.cpu.abi`）。arm64 上的 PRoot 尚未真机验证。
- 部分厂商系统会限制前台服务或后台网络，需要在系统设置中允许应用后台运行和自启动。
- 首次安装 Ubuntu 期间界面没有进度提示，属于已知的待改进项。

## 后续工作

- Ubuntu 环境安装进度和状态的界面提示。
- 正式签名密钥、版本号和分发渠道。
- iOS：需要 Mac 和 Xcode；入口与配置已具备，本机模式在 iOS 上不能执行命令。
