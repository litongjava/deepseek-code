# Android 真机测试交接

日期：2026-11-06。本文给在另一台电脑上用 Android 真机测试的人。功能说明见文档站“在浏览器和手机上使用”，架构与进展见 `docs/platform-feasibility.md`。

## 当前状态

已完成并在 Android 14 x86_64 模拟器（调试版）上验证：

- 本机模式：应用进程内运行 Bridge 和引擎，首次打开直接进入应用，无需服务器。
- Ubuntu 环境：首次启动后台下载 Ubuntu 24.04 base（约 30 MB，校验 SHA-256），模型的 Bash 命令通过 PRoot 在其中执行；`apt-get install git python3` 后可正常使用。
- 前台服务：任务进行时通知栏显示“正在本机运行”，切到后台继续执行。
- 远程模式：用户菜单“连接服务器”，填写 dcd-server 地址和访问令牌；登录页“在本机运行”切回。
- 界面避开状态栏、导航栏和键盘；设置页、供应商编辑在窄屏上分行排列。

发布版（release）在模拟器上验证了：安装、启动后连上应用内引擎、前台服务运行、设置页布局。发布版的对话、Ubuntu 命令和 apt 安装没有在本机跑完：模拟器在本机内存和 CPU 不足，系统界面长时间无响应、adb 频繁断开。

补充真机验收：OnePlus 12（Android 16、arm64、4 KB 页大小）调试包已验证本机真实问答、默认 Bash 执行、Ubuntu 交互终端、快速连续输入、Ctrl+C、关闭确认、键盘避让和横竖屏切换。手机切到后台后，60 秒命令继续执行并返回结果。远程模式通过 USB 转发连接隔离的电脑服务器，完成真实问答；切回本机后保留手机历史会话。

手机的 Ubuntu 中已通过国内镜像安装 Node.js、npm、Python、Git 和 curl，并实际运行 Node/Python 脚本及 curl HTTPS 请求。软件安装在应用数据中，不包含在基础 APK 内，更新 APK 后保留。APT 保留仓库签名校验；基础环境没有 CA 证书时，先通过已签名的 HTTP 软件源安装 `ca-certificates`，之后再切换 HTTPS 镜像。

常用网络、文件处理和开发工具已补齐，安装清单见文档站“在手机上使用”。真机验证 curl、wget 的 HTTPS 请求、dig DNS 查询、jq 解析、pip 和 Python venv 正常。`ip addr show lo` 可读取回环地址；Ubuntu 中 ping 的权限调用、ss 的 netlink 访问和 ifconfig 的部分网络信息读取受到 Android 权限限制。BusyBox ping 同样受限，Android 原生 ping 在应用身份下成功；验收时应记录实际权限结果，不能仅凭命令已安装判定网络诊断全部可用。

真机尚未覆盖：release 构建、16 KB 页大小设备、正式签名和分发，以及公网 HTTPS 服务器连接。

## 拿到 APK

方式一：使用已构建的测试包。开发目录 `runs/android-release/` 下的 `release-test-arm64.apk` 是同时包含 arm64-v8a 和 x86_64 的发布版，用测试密钥签名（只用于测试，不能发布）。复制到测试电脑后：

```bash
adb install -r release-test-arm64.apk
```

方式二：自己构建。当前 Android 工程要求 SDK Platform 36；另需 Node.js 22、Rust（带 `aarch64-linux-android` 目标）、Android SDK build-tools、NDK r27、JDK 17 或 21。PRoot 准备脚本使用 Linux 版 `patchelf` 和 `ar`，应在 Linux 或 WSL 中执行。设置 `ANDROID_HOME`、`NDK_HOME`、`JAVA_HOME` 后：

```bash
npm ci
npm run android:proot                                  # 下载并校验 PRoot 组件
npx tauri android build --apk --target aarch64         # 发布版，未签名
npx tauri android build --apk --debug --target aarch64 # 调试版，可用 run-as 查看数据目录
```

Windows 可以分两步构建：在仓库根目录用 `wsl bash scripts/fetch-android-proot.sh` 准备原生组件，再在配置好 Windows SDK、NDK 和 JDK 的 PowerShell 中执行上面的 `npx tauri android build`。两步使用同一份工作树。先运行 `rustup target add aarch64-linux-android` 安装目标；有多个 WSL 发行版时用 `wsl -d <发行版> bash scripts/fetch-android-proot.sh` 指定。脚本与 `gradlew` 已通过 Git 属性固定 LF 换行；旧工作树若出现 `pipefail` 后带回车的错误，需将相应文件换行为 LF。

Windows 的标准打包流程需要创建符号链接的权限。若 Rust 已显示 `Finished`，随后在链接 `libdeepseek_desktop_tauri_lib.so` 时提示 `Creation symbolic link is not allowed`，可以由维护者开启 Windows 开发者模式后重试。也可针对这次已完成的 arm64 调试构建，复制产物后调用 Gradle：

```powershell
Copy-Item target/aarch64-linux-android/debug/libdeepseek_desktop_tauri_lib.so src-tauri/gen/android/app/src/main/jniLibs/arm64-v8a/
Push-Location src-tauri/gen/android
./gradlew.bat :app:assembleArm64Debug -x :app:rustBuildArm64Debug --console=plain
Pop-Location
```

该路径跳过 Rust 编译，只适用于刚刚成功编译的产物；更改 Rust 或前端后必须重新编译再复制，不能用旧库代替新构建。APK 输出在 `app/build/outputs/apk/arm64/debug/`。

交接中的 `runs/android-release/` 是原构建电脑的本地产物，不随 Git 同步。新电脑先确认 APK 是否存在，没有时按上述步骤构建。

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
6. 让模型执行 `apt-get update && apt-get install -y git nodejs npm python3 && git --version && node --version && npm --version && python3 --version`（超时设为 600000）：成功输出版本号。下载慢时可将 `etc/apt/sources.list.d/ubuntu.sources` 中的 Ubuntu ports 地址切换到国内镜像，保留发行套件、组件和签名密钥配置。
7. 发送一个耗时任务（如 `sleep 60; echo done`），立刻切到其他应用：通知栏显示“正在本机运行”，1 分钟后回来，任务已完成。从最近任务划掉应用后，通知消失。
8. 连续执行多条命令，确认聊天中显示可折叠的 `Ran commands` 分组。
9. 旋转屏幕、弹出键盘：顶部不被状态栏遮挡，输入框不被键盘挡住。
   打开右侧终端面板，新建两个终端，执行 `head -2 /etc/os-release`、`pwd`；Ubuntu 就绪后，交互终端与模型 Bash 工具共享环境和工作区绑定。验证手机快捷键栏的 Tab、方向键和 Ctrl+C，再验证关闭确认的取消与确认。
10. 远程模式：电脑运行 `dcd-server --host 0.0.0.0 --port 8443 --tls-cert <证书> --tls-key <私钥>`（发布版只允许 HTTPS 远程地址；调试版可用局域网 HTTP），手机用户菜单“连接服务器”，填写地址和 `dcd-server --print-token` 的令牌，能对话；登录页“在本机运行”切回后原会话仍在。

## 可能遇到的问题

- 首次发送就提示会话被其他执行者占用：检查文件锁兼容性。Android 的标准库文件锁可能返回 `Unsupported`；共用引擎现在通过统一文件锁入口使用 Android 系统 `flock`，保留独占和关闭即释放的行为。真机测试需同时覆盖无竞争的首次问答和竞争者被拒绝，不能删除锁文件来绕过所有权检查。
- Windows 能看到 ADB Interface，但 `adb devices -l` 列表为空：先检查设备管理器中该接口的驱动和设备实例。在一次 OnePlus USB 接入中，WinUSB 接口缺少 `DeviceInterfaceGUIDs`；按 Android 官方驱动定义补充 ADB GUID 后重启该设备接口，再重启 ADB 服务即可识别。该修复需要管理员权限，修改前备份对应设备项，只处理实际连接的 ADB 接口，不替换其他 USB 驱动。GUID 以 [Android 官方驱动定义](https://android.googlesource.com/platform/development/+/master/host/windows/usb/android_winusb.inf) 为准。若设备显示 `unauthorized`，解锁手机并允许 USB 调试。
- 第 5 步一直不是 Ubuntu：检查网络能否访问 `cdimage.ubuntu.com`；调试版看 logcat 中 `LinuxEnv` 的报错。安装失败会在下次启动重试。
- 第 5 步报 PRoot 错误：记录 logcat 和设备型号、Android 版本、CPU 架构（`adb shell getprop ro.product.cpu.abi`）与页大小（`adb shell getconf PAGESIZE`），方便对比已验证设备。
- 部分厂商系统会限制前台服务或后台网络，需要在系统设置中允许应用后台运行和自启动。
- 首次安装 Ubuntu 期间界面没有进度提示，属于已知的待改进项。

## 后续工作

本轮共用引擎已统一 Android 文件锁入口，并让默认 Shell 识别 Ubuntu 与安装前的系统 Shell；交互终端沿用相同的环境变量和目录绑定。移动端使用具有充足线程栈的共用运行时来执行工具，终端输入按会话顺序发送，保证长命令字符顺序。手机界面优化已合并到 `main`；合并后的 Android 编译与调试包构建已通过。

- Ubuntu 环境安装进度和状态的界面提示。
- 正式签名密钥、版本号和分发渠道。
- iOS：需要 Mac 和 Xcode；入口与配置已具备，本机模式在 iOS 上不能执行命令。
