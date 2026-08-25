# Computer Use 隔离设计

## 先说结论

“同一个桌面里再画一只鼠标”只是视觉效果，不是输入隔离。只要事件仍发给宿主 compositor、WindowServer 或 Win32 输入队列，它就可能改变用户的 hover、焦点、拖拽和按键目标。

YujialeCode 因此按以下优先级路由：

1. 能用控件语义动作时，直接调用控件的 `invoke`、`set value`、`scroll`，不生成物理输入。
2. 需要像素坐标或任意 GUI 时，在独立 compositor / VM 内运行应用。
3. 纯网页任务使用独立 Chromium/CDP。
4. 只有用户明确传入 `backend=host` 才允许控制真实桌面。

## 成熟项目和系统接口给出的共同答案

### Linux / Wayland

- OpenAI Computer Use 官方指南建议把代理放进隔离浏览器或 VM；完整桌面示例使用容器、Xvfb、VNC 和浏览器，而不是与用户共用输入设备：<https://developers.openai.com/api/docs/guides/tools-computer-use>
- `headless-wayland-harness` 用独立 headless Sway 驱动 GUI。它验证了一个容易忽略的问题：无头 seat 必须在整个生命周期持续拥有虚拟指针和虚拟键盘，否则 GTK/winit 等客户端不会绑定输入能力：<https://github.com/tidynest/headless-wayland-harness>
- Weston 官方支持 nested Wayland/X11、headless、RDP 和 VNC 后端，证明“嵌套 compositor”是 Wayland 体系内的标准隔离边界：<https://wayland.pages.freedesktop.org/weston/toc/running-weston.html>
- wayvnc 可为 wlroots compositor 创建虚拟输入设备，也能连接 headless output；连接用户正在使用的 compositor 仍会影响该会话，因此它只适合作为独立 compositor 的远程通道：<https://github.com/any1/wayvnc>
- AT-SPI 的 Action 接口允许辅助技术直接调用控件动作，适合未来的 Linux 语义后端：<https://gnome.pages.gitlab.gnome.org/at-spi2-core/libatspi/iface.Action.html>
- XDG RemoteDesktop Portal / libei 是经过授权的远程输入通道，但目标仍是当前桌面会话，不等于另一个独立鼠标：<https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html>

Wayland 普通客户端不能像 X11 那样任意读取其他客户端的私有 surface。这是安全模型，不是缺少一个截图命令。想要后台像素级控制任意应用，必须获得 compositor 特权，或把应用放到自己的 compositor 中。

### Windows

- Microsoft `winappCli` 默认用 UI Automation 的控件 pattern 做 `invoke`、`set-value`、`scroll`，这些动作不注入鼠标；窗口截图使用 Windows Graphics Capture，被其他窗口遮挡时也可工作：<https://github.com/microsoft/winappCli/blob/main/docs/ui-automation.md>
- UI Automation Control Patterns 是 Windows 官方的控件语义层：<https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-controlpatternsoverview>
- Windows Graphics Capture 可针对指定 HWND 创建 capture item：<https://learn.microsoft.com/en-us/windows/win32/api/windows.graphics.capture.interop/nf-windows-graphics-capture-interop-igraphicscaptureiteminterop-createforwindow>
- Microsoft UFO 同样以 Windows UI Automation 为核心，而不是把 `SendInput` 冒充后台鼠标：<https://github.com/microsoft/UFO>

Windows 的 `SendInput`、鼠标 hover、拖拽和键盘仍属于交互桌面；`PostMessage/SendMessage` 也不是通用鼠标，很多现代 UI、安全边界和游戏会忽略它。完整像素隔离应使用 Windows Sandbox/Hyper-V/VM；同一会话优先使用 UIA + WGC。

### macOS

- `SCContentFilter(desktopIndependentWindow:)` 可只捕获指定窗口，不依赖当前桌面焦点：<https://developer.apple.com/documentation/screencapturekit/sccontentfilter/init%28desktopindependentwindow%3A%29>
- AXUIElement 提供控件树、属性和值以及 `AXUIElementPerformAction`，适合后台语义操作：<https://developer.apple.com/documentation/applicationservices/axuielement_h>

CoreGraphics 生成的 CGEvent 仍进入全局会话，会抢用户的指针/键盘。macOS 完整像素隔离应使用虚拟机；同一会话优先使用 AXUIElement + ScreenCaptureKit。

### 浏览器

CDP、WebDriver 和 Playwright 都把输入发给独立浏览器 target，不依赖宿主鼠标。YujialeCode 直接使用 CDP，是为了减少本地模型参数量和额外运行时：

- `Page.captureScreenshot` 获取页面像素；
- `Input.dispatchMouseEvent` 驱动浏览器内部指针；
- `Input.insertText` / `Input.dispatchKeyEvent` 输入；
- 每个会话使用独立 profile、loopback 调试端口和生命周期。

## YujialeCode 当前实现

| backend | 用途 | 是否动宿主鼠标 | 是否需要宿主焦点 |
| --- | --- | --- | --- |
| `isolated`（默认） | Linux 任意 GUI，独立 headless Sway | 否 | 否 |
| `browser` | Windows/macOS/Linux 网页，独立 Chromium/CDP | 否 | 否 |
| `host` | 兼容旧流程，控制当前真实桌面 | 是 | 部分动作需要 |

Linux `isolated` 的生命周期：

1. 在 `~/.yjlcoder/computer-use/desktop/<session>/runtime` 创建 0700 的独立 runtime 目录。
2. 启动 `WLR_BACKENDS=headless` 的 Sway，独占自己的 Wayland socket。
3. Rust 线程连接该 socket，持续持有 `zwlr_virtual_pointer_v1`。
4. 持续运行一个虚拟键盘 keeper，让 seat 始终公布 pointer + keyboard 能力。
5. `launch` 出来的每个程序只获得隔离 socket；`grim`、`wtype` 和 `swaymsg` 也只连接它。
6. `stop` 或 YujialeCode 退出时清理应用、输入设备和 compositor。

## 平台边界和后续工作

- Linux 当前已经完成任意 GUI 的 compositor 隔离，并在 Niri 实机验证：启动 Firefox、截图、点击、打字和回车后，宿主 `focused-window.id` 保持不变。
- Windows 下一阶段应移植 `winappCli` 的 UIA pattern 路由与 WGC 窗口捕获；真实输入只能作为显式 host 降级。
- macOS 下一阶段应实现 AXUIElement tree/action/value 与 ScreenCaptureKit 单窗口截图；CGEvent 只能作为显式 host 降级。
- Windows/macOS 若要求对任何不提供可访问性树的应用做像素级点击，必须把应用放进 VM，不能承诺同一用户会话的“完美第二鼠标”。
