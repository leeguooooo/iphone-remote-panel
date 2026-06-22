<p align="center">
  <img src="assets/icon-1024.png" alt="iphone-use 图标" width="120">
</p>

<h1 align="center">iphone-use</h1>

<p align="center"><em>把 computer-use 搬到 iPhone 上 —— 让 AI 智能体（和你的浏览器）能「看见」并「操作」一台真实的手机。</em></p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT"></a>
  <img src="https://img.shields.io/badge/platform-macOS%2015%2B-lightgrey" alt="Platform: macOS 15+">
  <img src="https://img.shields.io/badge/built%20with-Rust-orange" alt="Built with Rust">
  <img src="https://img.shields.io/badge/streaming-WebRTC%20%2F%20H.264-success" alt="Streaming: WebRTC / H.264">
</p>

<p align="center">
  <strong>简体中文</strong> ·
  <a href="README.md">English</a>
</p>

<p align="center">
  <img src="assets/hero.png" alt="在浏览器里操控 iPhone —— 实时画面加一排触控工具栏（主屏、聚焦搜索、App 切换、键盘）" width="320">
</p>

**在任意浏览器里远程操控你的 iPhone** —— 基于 macOS 的 **iPhone 镜像（iPhone Mirroring）**，
低延迟 WebRTC 画面 + 接近原生的触控体验。一个 Rust 守护进程用 **ScreenCaptureKit** 抓取镜像窗口，
用 **VideoToolbox** 硬件编码成 **H.264**，再通过 **WebRTC** 推流到 iPhone Safari（或任意浏览器），
同时把点按、滑动、滚动、文字作为连续的系统事件注入回去。AI 智能体、脚本、机器人也能通过一套简单的
**HTTP API** 操作同一台手机。

可以把它理解成 **「给 iPhone 用的 Chrome 远程桌面」** —— 全程跑在你自己的 Mac 上，不经过任何第三方云。

## 功能特性

- 📱 **在浏览器里操控 iPhone** —— 实时画面 + 点按 / 滑动 / 滚动 / 输入，iPhone Safari 或任意桌面浏览器都行。
- ⚡ **低延迟** —— 硬件 H.264（VideoToolbox）走 WebRTC，而不是截图轮询。
- 🤚 **接近原生的触控** —— 真实的滚轮滚动、keycode 文字输入、主屏 / 聚焦搜索 / App 切换快捷操作。
- 🤖 **为智能体而生** —— 一套 HTTP API（`/agent/input`、`/agent/screenshot`）让 AI 智能体和脚本既能「看」也能「操作」手机。
- 🌐 **局域网或远程** —— 同一 Wi-Fi 下走局域网，或通过 Cloudflare 隧道 + TURN 从任意网络接入。
- 🔒 **自托管 + 鉴权** —— 密码登录；跑在你自己的机器上，画面永远不离开你的掌控。

> v2 —— 在 v1（截图轮询服务）基础上彻底重写：WebRTC + 硬件编解码 + 连续输入。
> 输入 + 视频这条主链路（视频、点按、滚动、文字、快捷操作、局域网 WebRTC）已在真机上验证通过。

## 架构

![架构图](assets/architecture.png)

Rust 守护进程用 **ScreenCaptureKit** 抓取 macOS 的 iPhone 镜像窗口，用 **VideoToolbox** 硬件编码成
**H.264**，再通过 **WebRTC** 推流（`webrtc-rs`，HTTP/WS 信令用 axum）。同一套「抓取 / 输入」内核同时服务两类前端：
**人类客户端**（iPhone Safari —— 实时画面 + 连续触控）和 **智能体客户端**（一套 HTTP 控制 API，见 [智能体 API](#智能体-api)）。
触控通过系统 HID 事件链路以连续的 `CGEvent` 注入回去。大部分 NAT 由 STUN 打通；剩下的由可选的 Cloudflare TURN 中继。

写进守护进程的几条关键输入经验（全部经过真机验证）：

- **滚动是滚轮事件。** iPhone 镜像会把鼠标拖拽当成长按 / 图标排序，永远不会滚动 ——
  手指滑动必须映射成 `CGEvent` 滚轮事件。
- **文字是 keycode，不是 Unicode。** 镜像转发的是虚拟 keycode（以及一个*真实*的 Shift 键），
  而不是 `CGEvent` 的 Unicode 负载。**中文注意事项：** 输入走的是美式 keycode；如果手机键盘是中文（拼音）输入法，
  数字会被当成候选词序号（`a1b2c3` → `啊不c3`）—— 输入纯英文/数字时先把手机切到英文 ABC 键盘。
  真正的中文输入需要手机端的输入法，暂不在范围内。
- **HID 点按要求镜像窗口在最前。** 只有当其它 App 抢走焦点时，守护进程才会重新把焦点夺回来。

### 部署 —— 一个跑在登录会话里的 LaunchAgent

![部署图](assets/deployment.png)

ScreenCaptureKit（屏幕录制）和输入注入（辅助功能）需要 TCC 授权，而授权绑定在**登录会话内**的已签名身份上 ——
通过 SSH 启动的进程会被拒绝。所以守护进程以一个已签名的 **LaunchAgent** 运行在桌面会话里，只需授权一次；
之后 SSH 终端、智能体、iPhone Safari 控制端都**连接到它**。

### 控制租约 —— 一个光标，一个控制者

![控制与输入](assets/control-input.png)

HID 点按驱动的是宿主 Mac 上**唯一的真实光标**，且要求镜像窗口在最前。一个强制的**控制租约**在同一时刻只把这个光标授予一个控制者
（人或智能体）；最近操作的一方持有控制权。没有租约的话，人和智能体会因为抢同一个光标而互相搞乱手势。
纯观看者（只消费 WebRTC 画面、不发输入）不受影响：输入是「最后连接者获胜」，但所有观看者都保留各自的视频流。

## 环境要求

- macOS 15 Sequoia 或更高（iPhone 镜像本身的要求），并且已设置好并登录 **iPhone 镜像**。
  已在 macOS 15 Sequoia / 26 Tahoe 上验证；macOS 27 的支持见[路线图](#路线图)。
- Rust 工具链（用于构建）—— `cargo`。
- **零外部运行时依赖** —— 所有输入（点按、滚动、文字、按键、快捷操作）都通过原生 `CGEvent` 直接注入，
  截图用系统自带的 `screencapture` 命令。运行时不需要任何第三方二进制（`cua-driver` 之类都不需要）。
- *（可选）* 一个 Cloudflare TURN key，用于跨网络（蜂窝 / 远程）访问。

## 安装

构建、打包成签名 `.app`、并注册 LaunchAgent：

```bash
cargo build --release --bin iphone-use
./scripts/make-app.sh                 # → ./iPhoneUse.app
./install.sh ./iPhoneUse.app          # 签名、安装、写入 LaunchAgent
```

`install.sh` 会绑定 `0.0.0.0`、生成一个密码（或使用 `$PHONE_REMOTE_PASSWORD`），
打开「屏幕录制」+「辅助功能」面板让你授权一次，并打印出 iPhone 的连接地址。
在 iPhone 上（同一 Wi-Fi）打开 **`http://<mac的局域网IP>:44321/phone`** 并输入密码即可。

**预编译二进制**在每次打 version tag 时由 CI 发布 —— 见 [Releases 页面](../../releases)。
`install.sh` 会用 `codesign -s -` 在本地做 ad-hoc 签名；除非做了公证，否则 Gatekeeper 会弹一次确认。

> **从 v0.1.0 升级请注意：** v0.2.0 起 bundle id 从 `work.pwtk.iphone-remote` 改成了
> `com.leeguoo.iphone-use`，app 也从 `iPhoneRemote.app` 改名成 `iPhoneUse.app`。
> 因为 TCC 授权绑定 bundle id，**升级后必须重新授权「屏幕录制」和「辅助功能」**，旧授权不会自动继承。

### 不安装直接跑（开发用）

```bash
PHONE_REMOTE_HOST=0.0.0.0 PHONE_REMOTE_PASSWORD=secret \
  ./target/release/iphone-use serve
```

### 升级

daemon 每天检查一次 GitHub Release,结果体现在 `GET /agent/status`
(`version` / `latest` / `update_available`),落后时网页端会显示升级提示条。
升级命令与安装相同(bundle id 不变,TCC 授权保留):

```bash
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh   # daemon
npx skills update -g                                                                      # agent skill
```

离线/隐私环境可用 `PHONE_REMOTE_NO_UPDATE_CHECK=1` 关闭检查。

### 反馈 —— 欢迎人类,更欢迎 agent

用着别扭?[提个 issue](https://github.com/leeguooooo/iphone-use/issues)。
**明确鼓励 AI agent 来提**:自带的 skill 会指导 agent 在使用 API 受阻时
(报错误导、能力缺失、文档与实际不符)征得用户同意后提交结构化 issue。
最重度的用户的吐槽,才是产品进化的燃料。

## 配置（环境变量）

| 变量 | 默认值 | 用途 |
|---|---|---|
| `PHONE_REMOTE_HOST` | `127.0.0.1` | 监听地址（局域网用 `0.0.0.0`）。 |
| `PHONE_REMOTE_PORT` | `44321` | 监听端口。 |
| `PHONE_REMOTE_PASSWORD` | *(无)* | 共享密码（cookie 登录 + 智能体 bearer 兜底）。 |
| `PHONE_REMOTE_AGENT_TOKEN` | *(无)* | 专用的智能体 bearer token。设置后，智能体 API **只**接受这个 token（密码不再能当 bearer）；不设置时密码兼作 bearer（兼容旧行为）。 |
| `PHONE_REMOTE_CF_TURN_KEY_ID` / `_API_TOKEN` | — | Cloudflare TURN key → 临时中继凭证，用于跨网络。 |
| `PHONE_REMOTE_WDA_URL` | *(无)* | L2 元素树控制：指向可达的 WebDriverAgent（推荐 `http://127.0.0.1:8100`，由 `scripts/setup-wda.sh` 的中继提供）。设置后 agent 的文字/点按自动路由到手机端元素层 —— 中文直通、按标签点按无需坐标、完全不碰 Mac 光标；不设 = 纯像素路径。 |
| `PHONE_REMOTE_TURN_URLS` / `_USERNAME` / `_CREDENTIAL` | — | 静态 TURN 服务器（Cloudflare 的替代方案）。 |
| `PHONE_REMOTE_AUTO_RESUME` | *(关)* | `1` = 实验性：watchdog 自动点击 Mirroring 的 Resume/Connect 按钮恢复暂停屏。默认关 —— 手机使用中时 macOS 不允许后台 agent 把 Mirroring 置前，无法做到可靠，改用 `mirror_state`/`hint` 提示你何时手动点。 |
| `PHONE_REMOTE_IDLE_RELEASE_SECS` | `300` | 空闲自动释放（仅 WDA 模式）：连续这么多秒没有任何 `/agent` 操作、也没有人在看实时画面时，守护进程会停掉手机上的 WDA runner 并 bootout 它的 KeepAlive LaunchAgent，把手机**交还给你正常使用** —— 没人远程控制时不再一直占着设备。下一次 `/agent/input`（或网页「重新连接」按钮）会自动重新拉起 WDA（约 30–90s；锁屏的话解锁一次）。空闲释放期间 `/agent/status` 返回 `"released":true`。设为 `0` 关闭（WDA 24/7 常驻，旧行为）。 |

## 智能体 API

智能体通过**连接到**运行中的守护进程来操作手机（绝不要自己起一个输入进程 —— macOS 会把子进程发出的事件视为不可信）。
Bearer 鉴权：`Authorization: Bearer <token>`，其中 token 在设置了 `PHONE_REMOTE_AGENT_TOKEN` 时用它，
否则回退到 `PHONE_REMOTE_PASSWORD`（兼容旧行为）。

| 方法 | 路径 | 用途 |
|---|---|---|
| `GET` | `/agent/status` | 鉴权 / 健康探测 + 可操作性：`{ok, phone_target, wda, drivable, mirror_state, hint, mode, viewer_count, …}`。 |
| `POST` | `/agent/input` | 一条控制消息：点按 / 滚动 / 文字 / 按键 / 快捷操作 / 收键盘（坐标归一化到 `[0,1]`）。 |
| `GET` | `/agent/screenshot` | 当前手机画面，PNG 格式（校验帧；可回退到手机端截图）。 |

判断能否操作要看 **`drivable`** 而非 `phone_target`：Mirroring 窗口可能在、但显示「Connection Paused」/「iPhone in Use」中间屏，此时点按打空。`mirror_state`（`active`/`paused`/`in_use`/`offline`）+ `hint` 告诉你怎么办（paused → 点 Resume；in_use → 锁屏；offline → 打开 Mirroring）。

完整参考：**[`docs/agent-api.html`](docs/agent-api.html)**。

```bash
HOST=http://<mac的局域网IP>:44321; AUTH="Authorization: Bearer $PW"
curl -s -H "$AUTH" "$HOST/agent/screenshot" -o screen.png
curl -s -H "$AUTH" -X POST "$HOST/agent/input" -d '{"type":"shortcut","name":"home"}'
curl -s -H "$AUTH" -X POST "$HOST/agent/input" -d '{"type":"tap","x":0.5,"y":0.3}'
curl -s -H "$AUTH" -X POST "$HOST/agent/input" -d '{"type":"keyboard"}'   # 收起键盘 (wda)
```

## MCP 服务器

[`iphone-use-mcp`](crates/mcp/README.md) 是一个 MCP stdio 服务器（`crates/mcp`），
把 MCP 客户端（Claude Desktop、Claude Code）桥接到守护进程的智能体 API。九个工具：
`phone_status`、`screenshot`、`elements`（UI 元素树）、`tap`、`tap_label`（按标签点按）、
`scroll`、`type`（WDA 在线时中文直通）、`key`、`shortcut`。两个环境变量：
`PHONE_REMOTE_URL`（默认 `http://127.0.0.1:44321`）和 `PHONE_REMOTE_TOKEN`（可选；对应守护进程侧的 `PHONE_REMOTE_AGENT_TOKEN`）。

加到你的 `claude_desktop_config.json`（或 Claude Code 的 MCP 配置）：

```json
{
  "mcpServers": {
    "iphone-use": {
      "command": "/path/to/iphone-use-mcp",
      "env": {
        "PHONE_REMOTE_URL": "http://127.0.0.1:44321",
        "PHONE_REMOTE_TOKEN": "<你的-agent-token>"
      }
    }
  }
}
```

完整的工具 schema 和构建说明见 [`crates/mcp/README.md`](crates/mcp/README.md)。

## 快捷指令桥接（实验性）

![快捷指令桥接](assets/shortcuts-bridge.png)

除了在 UI 上点按，智能体还能通过一个精心维护的桥接快捷指令直达 **iOS 原生 API** ——
电量、Apple 健康、定位、信息、HomeKit。守护进程按名字触发 **「iU Bridge」** 快捷指令（剪贴板动词 + 聚焦搜索），
快捷指令根据该动词分发到对应的原生操作，并把**结构化 JSON 回 POST 到 `/agent/inbox`** —— 拿到的是确定的数据，而不是靠刮屏。
这是一条*增量的*快速通道：UI 自动化（点按 / 滚动，任意 App）始终是通用兜底。
见 [`shortcuts/README.md`](shortcuts/README.md) 和 [`shortcuts/registry.json`](shortcuts/registry.json) 里的动词表。

## 智能体技能（Agent skill）

让任意支持 skills 的智能体（Claude Code 等）学会操作你的手机 —— 包括
**「视觉一次 → 脚本永久」**的方法论（第一次用视觉解决一个手机任务，之后冻结成可复用的一行命令脚本）：

```bash
npx skills add leeguooooo/iphone-use
```

> 用 `-g` 全局安装时,若 `skills` CLI 报
> `PromptScript does not support global skill installation`,这是无害的部分失败 ——
> PromptScript 只支持项目级 skill,所以它那一个目标被跳过,其余 agent(Claude Code 等)
> 照常安装成功。加 `-a claude` 指定单个 agent 即可消除该警告。

技能内容涵盖智能体 API、「看 → 操作 → 验证」循环、经真机验证的输入经验（滚动方向、keycode/输入法坑），
以及一个完整范例 —— 导出 Apple 健康全量数据（它没有 API；智能体在「健康」App 里点按操作，约 3 分钟后数据落到你的 Mac 上）。
见 [`skills/iphone-use/SKILL.md`](skills/iphone-use/SKILL.md)。

## 安全须知

本工具把对手机的实时操控暴露在网络上，请把 URL 和密码当作敏感凭证对待。

- 绑定到局域网时密码是强制的（`install.sh` 会强制要求）。
- 远程访问的 HTTPS 由 Cloudflare 隧道终结（守护进程只提供明文 HTTP 并读取 `X-Forwarded-Proto`）；
  会话 cookie 是 `HttpOnly` + `SameSite=Lax`。
- 暴露访问期间，不要让支付 App、私密聊天、2FA 验证码界面停留在屏幕上。
- 不用时停止 / 卸载 LaunchAgent。

## 路线图

已交付并在 macOS 15 Sequoia / 26 Tahoe 上真机验证：WebRTC 视频、点按、滚动、keycode 文字、快捷操作、
焦点鲁棒的输入、智能体 HTTP API、LaunchAgent 安装。接下来：

- [ ] **macOS 27「Golden Gate」支持。** macOS 27 让 iPhone 镜像窗口*可变宽高比地缩放*
  （还能渲染 iPad 布局），不再锁定竖屏。需要让窗口选取与宽高比无关（按「在屏 + 面积」排序，而非形状），
  在 27 beta 上重新验证抓取 + 输入，并加上新的 **控制中心** 快捷操作。目标：一个构建跑通 macOS 15 / 26 / 27。
- [x] **MCP 服务器** 封装智能体 API，让 MCP 客户端（Claude 等）把 `tap` / `type` / `scroll` / `screenshot` 当原生工具用。
- [ ] **跨网络验证** Cloudflare 动态 TURN 链路（铸造 + 刷新代码已就绪；需要一次真实 key 的非局域网端到端跑通）。
- [x] **基于 WebDriverAgent 的元素树控制（「L2」层）** —— 已交付并通过 *daemon 自身 API* 真机验证（iPhone 17 / iOS 27）。WDA 跑*在手机上*、驱动 iOS 辅助功能树，同一套 agent API 自动选最优路径：`{"type":"text"}` **中文直通**（像素路径的 keycode 会被拼音 IME 吃掉）、`{"type":"tap","label":"…"}` **按元素点按**（无坐标、不碰 Mac 光标）、`GET /agent/elements` 以文本返回 UI（比视觉便宜一个量级）、镜像断开时截图自动回退手机端抓取 —— **人拿着手机时 agent 依然能看能操作**。一键安装：`./scripts/setup-wda.sh`（需 Xcode）；全部真机验证过的坑见 **[`docs/wda-setup.html`](docs/wda-setup.html)**。
- [x] **CI 发布二进制** + 一行 `curl … install.sh | sh` 安装。
- [ ] 一个简短的 **演示**（GIF / 视频）：AI 智能体通过 API 操作手机。

欢迎 Issue 和 PR。

## 目录结构

- `crates/core` —— 抓取、编码、坐标/几何、输入注入、控制租约。
- `crates/server` —— `iphone-use` 守护进程：HTTP/WS、WebRTC、信令、智能体 API、TURN。
- `web/index.html` —— iPhone Safari 客户端（WebRTC 观看端 + 触控）。
- `install.sh`、`scripts/make-app.sh`、`deploy/` —— 打包 + LaunchAgent。
- `docs/` —— 设计规格、运行手册、智能体 API 参考、调研笔记。

## 许可证

[MIT](LICENSE)
