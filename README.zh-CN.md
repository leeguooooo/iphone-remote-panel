<p align="center">
  <img src="assets/icon-1024.png" alt="iphone-use 图标" width="120">
</p>

<h1 align="center">iphone-use</h1>

<p align="center"><em>给真实 iPhone 用的 computer-use：让 AI agent 和浏览器看见并操作手机。</em></p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="许可证：MIT"></a>
  <img src="https://img.shields.io/badge/platform-macOS%2015%2B-lightgrey" alt="平台：macOS 15+">
  <img src="https://img.shields.io/badge/built%20with-Rust-orange" alt="使用 Rust 构建">
  <img src="https://img.shields.io/badge/default-WDA%20direct-success" alt="默认后端：Direct WDA">
</p>

<p align="center">
  <a href="README.md">English</a> ·
  <strong>简体中文</strong>
</p>

<p align="center">
  <img src="assets/hero.png" alt="在浏览器中查看并操作 iPhone" width="320">
</p>

**在浏览器或 AI agent 里查看并操作真实 iPhone。** 默认的 `direct` 后端在手机上运行
WebDriverAgent（WDA）：守护进程把 WDA 的 MJPEG 画面代理到 `/agent/mjpeg`，
浏览器通过有确认响应的 `POST /control` 操作手机，agent 使用 `/agent/input`。

Direct 不依赖 macOS iPhone 镜像、屏幕录制、辅助功能权限、Mac 光标或前台窗口。
旧的 ScreenCaptureKit + CGEvent 路径只在显式设置
`PHONE_REMOTE_BACKEND=mirror` 时启用。

## 功能

- 📱 **手机端实时画面**：浏览器显示 `/agent/mjpeg` 的 WDA MJPEG；直播失败时降级为 PNG 静帧。
- 🤚 **手机端输入**：点按、拖动、长按、滚动和文字由 WDA 在 iPhone 上合成，不抢 Mac 光标。
- ✅ **每次输入都有结果**：`/control` 明确返回成功或错误，不把断开的数据通道当成功。
- 🤖 **Agent API**：`/agent/input`、`/agent/elements` 和 `/agent/screenshot` 支持脚本与 AI agent。
- 🔒 **自托管**：浏览器登录和 agent bearer token 由本机守护进程校验。

> 当前迁移状态：WDA 的元素树、文字、点按和截图组件已有真机记录。新的
> Direct 浏览器整条链路仍须完成本文后面的真机验收矩阵。源码、单测或 daemon 在线都不能代替真机证据。

## 架构

```text
浏览器 <── GET /agent/mjpeg ── iphone-use daemon ── 127.0.0.1:9100 ──┐
浏览器 ── POST /control ─────> iphone-use daemon ── 127.0.0.1:8100 ──┤ iPhone 上的 WDA
Agent  ── /agent/* ──────────> iphone-use daemon ── 127.0.0.1:8100 ──┘
```

完整的生命周期、失败状态、安全边界和真机验收设计见
**[`docs/direct-device-architecture.html`](docs/direct-device-architecture.html)**。

`scripts/setup-wda.sh` 编译并签名 WDA，启动 XCUITest runner，并通过 USB
`iproxy` 建立控制端口 `8100` 和画面端口 `9100` 的 Mac loopback 中继。
普通安装路径要求 USB，不会在 USB 不可用时自动切到 Wi-Fi 或 `socat`。
`socat` 只适合操作者明确配置的手动/实验路径。

Direct 会 fail closed：WDA 不可用时，控制请求返回错误，不会改走 iPhone 镜像或移动 Mac 光标。

### Legacy mirror 兼容后端

只有显式设置 `PHONE_REMOTE_BACKEND=mirror` 才启用旧链路。它用
ScreenCaptureKit 抓取 iPhone 镜像窗口、VideoToolbox 编码 H.264、WebRTC 传输画面，
并通过 CGEvent 注入输入。该后端需要：

- 已配置并连接 iPhone 镜像；
- 给 iPhoneUse 授予屏幕录制和辅助功能权限；
- 已登录 Aqua 会话，且镜像窗口可被置前。

`assets/` 里的旧架构、部署和输入图描述的是这个兼容后端，不是 Direct 默认路径。

## 前置条件

- macOS 15 或更高。
- **完整 Xcode.app**，不能只有 Command Line Tools。在
  Xcode → Settings → Accounts 登录 Apple ID 并选择开发团队。免费 Personal Team
  可以用，但 WDA 签名通常需要定期续装。
- iPhone 已开启**开发者模式**。
- iPhone 已与 Mac 配对并点过**信任**。首次和普通运行路径都使用 USB。
- 编译、启动和操作 WDA 时，保持 iPhone **解锁、亮屏、唤醒**。WDA 不能绕过 Face ID 或密码。
- 安装 `iproxy`：`brew install libimobiledevice`。
- 只有从源码构建 daemon 时才需要 Rust 工具链。

Direct 不需要 iPhone 镜像、屏幕录制或辅助功能权限。

## 安装

安装最新 GitHub Release，并注册当前用户的 LaunchAgent：

```bash
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
```

安装器默认写入 `PHONE_REMOTE_BACKEND=direct`、WDA/MJPEG loopback 地址，并把
WDA 设置脚本放到 `~/.iphone-use/setup-wda.sh`。安装器完成不代表真机已经跑通。
连接、信任并解锁手机后继续执行：

```bash
~/.iphone-use/setup-wda.sh doctor
~/.iphone-use/setup-wda.sh
~/.iphone-use/setup-wda.sh status
```

然后在浏览器打开 **`http://<Mac局域网IP>:44321/setup`**。内置连接向导会把
`/agent/status` 翻译成当前的 USB、信任、开发者服务、WDA 或外部主机阻塞项；
它不会自动断开 VPN、修改代理或替你运行设置。设备真正可操作后再进入 **`/phone`**。
出现登录页时输入安装器打印的密码。多台 iPhone 与同一台 Mac 配对时，要固定同一个
classic UDID：

```bash
export PHONE_REMOTE_UDID=00008…
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
WDA_UDID="$PHONE_REMOTE_UDID" ~/.iphone-use/setup-wda.sh
```

从源码安装：

```bash
cargo build --release --bin iphone-use --bin iphone-use-mcp
./scripts/make-app.sh
./install.sh ./iPhoneUse.app
```

预编译产物见 [GitHub Releases](https://github.com/leeguooooo/iphone-use/releases)。
安装后的 app 内含同版本 MCP bridge：
`~/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp`；Release 也会额外发布
带 SHA-256 校验的 universal 独立压缩包。
签名方式取决于后端：

- **Direct**：保留已有且有效的 app 签名。只有 staged app 无签名或签名无效时，
  才使用不改 keychain 的 ad-hoc 签名。Direct 不申请屏幕录制或辅助功能权限，
  因此不需要稳定 TCC 身份。
- **Mirror**：使用稳定的本地 `iPhoneUse Local Signing` 身份，让 mirror-only TCC
  授权尽量跨升级保留。稳定 signer 不可用时，安装器会警告后再退回 ad-hoc 签名。

升级仍使用同一条 `install.sh` 命令：

```bash
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
```

安装器先把 release tag 解析成一个确定的 commit，helper 和 skill 固定到该 commit；
daemon app 来自对应的 Release asset，并校验其发布 SHA-256。在替换 daemon 之前，
它会把 skill 安装到 `~/.agents/skills/iphone-use`，逐字节校验实际落盘内容和
Claude Code 发现链接。skill 下载、校验或安装失败都会中止升级；后续 daemon 步骤
失败时，会恢复旧 skill、发现目标和 skills lock。不需要另跑 skills CLI。

如需明确保留现有 skill：

```bash
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh \
  | IPHONE_USE_SKIP_SKILL=1 sh
```

这是降级安装：安装器不会声称新 daemon 与现有 skill 兼容。

升级迁移按已有配置判断：旧 plist 没有 backend 字段，但已有有效的 loopback
`PHONE_REMOTE_WDA_URL` 时迁移到 Direct；只有完全没有 WDA 配置的旧安装才保留
Mirror 兼容模式。显式配置的 backend 不会被改写。

### 开发运行

```bash
PHONE_REMOTE_BACKEND=direct \
PHONE_REMOTE_WDA_URL=http://127.0.0.1:8100 \
PHONE_REMOTE_WDA_MJPEG_URL=http://127.0.0.1:9100 \
PHONE_REMOTE_HOST=0.0.0.0 PHONE_REMOTE_PASSWORD=secret \
  ./target/release/iphone-use serve
```

## 配置

| 环境变量 | 默认值 | 用途 |
|---|---|---|
| `PHONE_REMOTE_BACKEND` | `direct` | `direct` = WDA 输入 + 手机端 MJPEG；`mirror` = 显式 legacy ScreenCaptureKit + CGEvent。 |
| `PHONE_REMOTE_HOST` | `127.0.0.1` | 监听地址；局域网访问使用 `0.0.0.0`。 |
| `PHONE_REMOTE_PORT` | `44321` | HTTP 端口。 |
| `PHONE_REMOTE_PASSWORD` | *无* | 浏览器登录密码；未设置专用 agent token 时兼作 bearer。 |
| `PHONE_REMOTE_AGENT_TOKEN` | *无* | 专用 agent bearer token。设置后，不再接受密码作为 bearer。 |
| `PHONE_REMOTE_UDID` | 安装器识别并持久化；否则未设置 | managed WDA 和破坏性设备命令使用的 canonical classic UDID。目标一旦配置，单次请求不能临时换机；要换目标须修改部署配置并重启。setup 时传同值 `WDA_UDID`。 |
| `PHONE_REMOTE_WDA_URL` | Direct 安装为 `http://127.0.0.1:8100` | WDA 控制 loopback。不可达时 Direct 输入失败，不回退到 Mac。 |
| `PHONE_REMOTE_WDA_MJPEG_URL` | Direct 安装为 `http://127.0.0.1:9100` | WDA MJPEG loopback；daemon 从 `/agent/mjpeg` 代理给已认证客户端。 |
| `PHONE_REMOTE_WDA_MANAGED` | Direct loopback 默认开启 | daemon 是否负责本地 WDA supervisor/relay 生命周期；远端 endpoint 必须由外部管理。 |
| `PHONE_REMOTE_IDLE_RELEASE_SECS` | `300` | 空闲后释放 WDA。`0` 表示不释放。 |
| `PHONE_REMOTE_CF_TURN_KEY_ID` / `_API_TOKEN` | — | 仅 legacy mirror/WebRTC 使用。 |
| `PHONE_REMOTE_TURN_URLS` / `_USERNAME` / `_CREDENTIAL` | — | 仅 legacy mirror/WebRTC 使用。 |
| `PHONE_REMOTE_AUTO_RESUME` | *关* | 仅 legacy mirror 使用的 Resume/Connect 实验。 |

## Agent API

Bearer token 放在 `Authorization: Bearer <token>`。设置
`PHONE_REMOTE_AGENT_TOKEN` 时使用它，否则兼容性回退到 `PHONE_REMOTE_PASSWORD`。

所有会改变状态的 POST 还必须携带精确请求头 `X-Phone-Control: 1`。它是在 bearer
或 session 鉴权之外增加的 CSRF/操作意图保护，不能代替鉴权。它适用于 `/control`、
`/agent/input`、`/agent/mode`、`/agent/inbox` 和 `/agent/inbox/drain` 的 POST
形式；GET 不需要。内置网页和 MCP client 会自动添加。

| 方法 | 路径 | 用途 |
|---|---|---|
| `GET` | `/agent/status` | 查看 backend、目标是否固定、`managed_wda`、`managed_wda_pending`、`recovery_owner`、WDA 状态、生命周期、viewer 计数和恢复提示。 |
| `GET` | `/agent/mjpeg` | WDA 实时 MJPEG；支持 bearer 或浏览器 cookie。 |
| `POST` | `/control` | 浏览器 Direct 输入；需要 cookie、mutation header 和有上限的 `ttl_ms`，成功体只有 `{"ok":true}`。 |
| `POST` | `/agent/mode` | 需要 mutation header，只恢复当前 backend：Direct 接受 `{"mode":"agent"}`，Mirror 接受 `{"mode":"mirror"}`；不会切换 backend 或 canonical UDID。 |
| `POST` | `/agent/input` | 需要 bearer 和 mutation header；支持点按、拖动、长按、滚动、文字、Home、Spotlight 和已有 named key。 |
| `POST` | `/agent/actions` | 仅 Direct/WDA 的有界多步执行；先校验完整批次，`wait_for` 做语义检查，任一步失败后续动作都不会发送。 |
| `GET` / `POST` | `/agent/inbox` | GET 只读查看 legacy Shortcuts 返回队列；POST 需要 bearer 和 mutation header，追加一条结果。 |
| `POST` | `/agent/inbox/drain` | 需要 bearer 和 mutation header；原子返回并清空队列。 |
| `GET` | `/agent/screenshot` | Direct 下只从 WDA 取当前目标手机 PNG。 |
| `GET` | `/agent/elements` | WDA 元素树与屏幕尺寸。WDA 缺失/繁忙返回 `503`，source 重试失败返回 `502`，不会用 `200` 空数组伪装成功。 |

Direct 自动化必须同时检查 `backend:"direct"`、`wda_actionable:true` 和
`drivable:true`。`device_state` 可能是 `ready`、`locked`、`blocked`、`offline`、
`releasing`、`released` 或 `reconnecting`。`phone_target`、`mirror_state` 和
`human_active` 是 legacy mirror 字段，不能证明 Direct 可控。managed loopback WDA 的
`recovery_owner` 是 `daemon`；首次本地接入尚未持久化设备目标时是 `unconfigured`，
远端或显式不托管的 endpoint 是 `external`。`viewer_count` 包含
`/ws` 和 MJPEG viewer，`mjpeg_viewer_count` 只统计 MJPEG。

Direct 输入按 at-most-once 处理。`/control` 使用必填的 1–2500 ms `ttl_ms`；
`/agent/input` 从服务端收到请求起使用固定 15 秒总 deadline。dispatch 前过期返回
`408`、`error:"not_sent"`、`retry_safe:true`。动作已经开始，但结果无法确认时返回
`error:"outcome_unknown"`、`retry_safe:false`：WDA/transport 明确报错但不能证明动作
未落地时是 `502`，post-dispatch deadline 是 `504`。遇到 502/504，先读 status 和
当前画面/元素；不要盲目重发文字、滚动、Back、点按或其他非幂等操作。
`/agent/actions` 最多接受 24 个 `action`、`wait_for` 或短 `pause` step，整个批次
持有一个 WDA 控制锁并在首个失败处停止。返回值包含 `completed`、
`applied_actions`、`failed_step`、`outcome` 和 `retry_safe`；只要前面已有动作落地，
就绝不会把“重放整个批次”标成安全。`tap_locator` 与 `wait_for` 使用相同的
label/identifier/kind/value/focused/enabled/visible 精确条件，并要求当前页面唯一命中，
让 durable locator 不只可检查，也能安全执行。

完整接口见 [`docs/agent-api.html`](docs/agent-api.html)；脚本化与竞品调研见
[`docs/scripted-flows-research.html`](docs/scripted-flows-research.html)。

```bash
HOST=http://<Mac局域网IP>:44321
AUTH="Authorization: Bearer $PW"
MUTATION="X-Phone-Control: 1"
curl -s -H "$AUTH" "$HOST/agent/status"
curl -s -H "$AUTH" "$HOST/agent/screenshot" -o screen.png
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"tap","x":0.5,"y":0.3}'
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"text","text":"你好"}'
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/actions" -d '{"steps":[{"kind":"action","action":{"type":"shortcut","name":"home"}},{"kind":"wait_for","expect":{"present":[{"label":"搜索"}]},"timeout_ms":3000}]}'
```

## MCP server

[`iphone-use-mcp`](crates/mcp/README.md) 提供 12 个 MCP 工具：
`phone_status`、`screenshot`、`elements`、`tap`、`tap_element`、`tap_label`、
`scroll`、`type`、`key`、`shortcut`、`phone_run_steps` 和 `phone_reconnect`。`tap_element` 必须使用
同一次 `phone_elements` 返回的 index 和 snapshot；`tap_label` 只在精确标签唯一时
执行，零匹配或重名都不会发送动作。`phone_reconnect` 无参数，只恢复持久化的
canonical managed Direct/WDA 目标；它不接受 UDID、不换设备，也不回退 Mirroring。
`phone_run_steps` 可在一次 MCP 调用里组合稳定动作、picker 和语义等待，完整批次由
daemon 预校验并失败即停。MCP client 会给它发出的状态变更请求自动添加
`X-Phone-Control: 1`。MCP 没有通用 mode 切换。long-press、swipe、drag 也可作为
`phone_run_steps` 的 step；键盘收起、卸载仍走 HTTP API。按格式校验 bundle id
的 App 启动可作为 `launch_app` step。
主要配置：

```json
{
  "mcpServers": {
    "iphone-use": {
      "command": "/Users/YOUR_ACCOUNT/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp",
      "env": {
        "PHONE_REMOTE_URL": "http://127.0.0.1:44321",
        "PHONE_REMOTE_TOKEN": "<agent-token>"
      }
    }
  }
}
```

正常安装器会把同版本 daemon、MCP 与 agent skill 一起交付；请把
`YOUR_ACCOUNT` 替换为 macOS 账户名。工具结构和独立安装方式见
[`crates/mcp/README.md`](crates/mcp/README.md)。

同一个二进制也能直接重放审阅过的 flow，正常路径不需要模型参与：

```bash
MCP="$HOME/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp"
"$MCP" flow validate examples/flows/open-spotlight.json
PHONE_REMOTE_TOKEN="$PHONE_REMOTE_AGENT_TOKEN" \
  "$MCP" flow run examples/flows/open-spotlight.json
PHONE_REMOTE_TOKEN="$PHONE_REMOTE_AGENT_TOKEN" \
  "$MCP" flow run examples/flows/search-spotlight.json --input 'query=咖啡'
```

flow v1 是严格 JSON：包含 `version`、`name`、可选 `description`、显式 string
`inputs` 和与 `phone_run_steps` 相同的 guarded `steps`。`--input KEY=VALUE`
只在本次执行中解析参数，不会把值写回 JSON；流程也绝不会自动重试失败或结果未知的
批次。CLI 参数仍可能出现在 shell history 或进程信息里，因此不要用它传凭据、验证码、
隐私内容，也不要把支付、发送、发布或删除动作做成可复用 flow。

浏览器控制栏现在提供「多步」录制：只记录服务端确认成功的动作；从「控件」面板选择时
优先保存精确可访问性标签；动作后的元素树出现稳定变化时，录制器只会用新出现的唯一
可访问性 identifier 或前台应用追加 `wait_for` 语义检查点，而不是只依赖固定延迟。
自动检查点不会保存可能包含姓名、金额的任意画面标签和值。坐标点按、滑动、长按和拖动会明确标记
为易失；文字输入会转换成命名运行参数，刚才输入的原文会被丢弃，下载 JSON 只包含
参数定义。停止后可成组上移或删除动作及其检查点，填写本次运行值，再下载合法的
flow v1 文件或一次提交给 `/agent/actions`。遇到无法稳定录入的动作时，下载会明确标为
“不完整草稿”，且界面不会开放一次执行；没有缺口的流程也必须填写全部必需参数并先
勾选“不含支付、发送、发布、删除等不可逆操作”。

## Shortcuts bridge（legacy mirror 实验）

![Shortcuts bridge](assets/shortcuts-bridge.png)

现有 “iU Bridge” 通过 Mac 侧的 Spotlight、剪贴板和键盘事件启动，因此只属于
`PHONE_REMOTE_BACKEND=mirror`。Direct 已支持 Home、Spotlight 和文档列出的
WebDriver named key；App Switcher、Control Center、Shortcuts bridge 和任意 Mac
合成键码仍不属于 Direct 已承诺能力。

旧桥接说明保留在 [`shortcuts/README.md`](shortcuts/README.md) 和
[`shortcuts/registry.json`](shortcuts/registry.json)。

## Agent skill

正常的 `install.sh` 会从 daemon 所属 release 的确定 commit 安装 skill 到全局标准目录，
校验实际内容和 Claude Code 发现链接，并把 daemon 与 skill 作为同一个事务升级。以后
仍然重跑安装器，不要单独从移动来源更新 skill。本地开发使用
`./install.sh /path/to/iPhoneUse.app` 时，则安装当前工作树里的 skill。

skill 覆盖 agent API 和“看 → 操作 → 验证”循环。依赖 Shortcuts bridge、App Switcher、
Control Center 或任意 Mac 键码的旧示例都按 legacy 处理。Home、Spotlight 和已列出的
named key 使用 Direct/WDA。详见 [`skills/iphone-use/SKILL.md`](skills/iphone-use/SKILL.md)。

## 网络与安全

daemon 的密码/cookie/bearer 只保护 `44321` 上的浏览器和 agent API。
**WDA 自己的设备端 `8100` 和 `9100` 没有认证。**

USB `iproxy` 的作用是让 daemon 固定连接 `127.0.0.1`，并把 relay 绑定到指定 UDID。
它不会给 iPhone 上的 WDA 加认证，也不能阻止同一局域网里的其他机器直接访问手机 IP
上的 `8100/9100`。Phase 1 只适合可信、隔离的网络；不要在访客 Wi-Fi、公共网络或
不受控办公网运行 WDA。必要时关闭手机 Wi-Fi，只保留 USB。

真正的设备传输认证边界属于 Phase 2：由 companion app 或受控 tunnel 提供认证、
加密和明确的授权生命周期。在这之前，不要把 daemon 的登录密码误当成 WDA 端口的保护。

- `install.sh` 在 daemon 绑定局域网时强制要求密码。
- 远程访问 daemon 时，应使用自己管理的 HTTPS tunnel。
- 不要在开放访问期间停留在支付、私聊或 2FA 画面。
- 不使用时停止 WDA/LaunchAgent。

## WARP / VPN

WARP 或其他 VPN 可能阻断 CoreDevice 隧道，导致 WDA 无法安装、启动或恢复。
`setup-wda.sh doctor` 只负责检测并给出提示，**不会自动断开或恢复 WARP/VPN**。
网络策略由操作者决定。daemon 自己管理的恢复因此受阻时，`/agent/status` 会报告
`device_state:"blocked"`、`setup_blocked_on:"warp"` 和可执行的恢复提示，而不是
泛泛地要求继续等待。没有阻塞项但仍在恢复时，`setup_phase` 和
`setup_message` 会报告当前构建阶段；只有 `drivable:true` 才代表可以操作。
企业设备应由管理员配置合适的 split tunnel。

**WARP 同样会打挂 iPhone 镜像本身——哪怕本项目一行都没跑。** 镜像走的是接力（Continuity），
常开 VPN 会同时拖累 Wi-Fi 关联和 CoreDevice 隧道，于是在 daemon 已停、WDA 根本没装的情况下，
镜像窗口照样卡在「连接中」或直接超时（issue #17，在 macOS 26 与 27.0 beta 上各自独立复现）。
往这里提 bug 之前先自查：

1. 把我们的东西全停掉 —— `launchctl bootout gui/$(id -u)/com.leeguoo.iphone-use`
   以及 `.wda` 那个 job —— 并退出镜像窗口。
2. `warp-cli disconnect`（或彻底退出 VPN 客户端）。
3. 重新打开 iPhone 镜像。

第 3 步能连上，就说明 daemon 从头到尾没参与，解法与上面的 split tunnel 排除一致。
注意 Zero Trust 的 *Always On* 策略会自动把 WARP 重新连上，所以长期解法是让管理员配置排除规则，
手动断开只能顶一时。

## 路线

- [ ] 按下面的矩阵完成 Direct 浏览器整链路真机验收。
- [ ] 补齐签名续期、睡眠/重连、`releasing` / `reconnecting` 状态和多设备恢复体验。
- [ ] 逐项重验 Direct 命令，不继承 legacy Mirroring 的能力名称。
- [x] MCP server。
- [x] WDA 元素树、Unicode 文字、label 点按、坐标点按和手机端截图已有组件级真机记录。
- [x] GitHub Release 二进制和 `install.sh` 安装。
- [ ] Phase 2 companion/tunnel：为设备画面和控制补上认证传输，再评估替换 WDA MJPEG。

## 真机验收边界

只有下面各项都在目标 iPhone 上观察到，才能把新默认标成“已验收”：

1. Mac 不给 iPhoneUse 屏幕录制/辅助功能权限，也不打开 iPhone 镜像，Direct daemon 仍启动。
2. `/agent/status` 报告正确 UDID、`backend:"direct"`、`wda_actionable:true` 和 `drivable:true`。
3. 另一台设备打开 `/phone`，MJPEG 持续更新；停掉 9100 relay 后，页面明确显示降级/离线。
4. 浏览器 `/control` 的点按、拖动、长按、上下滚动、ASCII/CJK、Home、Spotlight 和 named key
   各执行一次并收到确认；App Switcher 要诚实失败。
5. `/agent/elements`、`/agent/screenshot`、`/agent/input` 的 bearer 鉴权和 WDA 故障路径符合文档，
   任何失败都不能移动 Mac 光标。
6. 验证 `releasing` → `released` → `reconnecting` → `ready`，并覆盖锁屏/解锁、USB 重连、
   daemon/Mac 重启、WDA 重签和多设备不串机。
7. 在隔离网络检查手机 IP 的 `8100/9100` 暴露情况，并把观察结果记入验收记录。

在这份记录完成前，本文描述的是目标默认和已实现接口，不代表 `install.sh` 已经通过完整真机链路。

## 目录

- `crates/server`：Direct WDA 控制、MJPEG 代理、浏览器/agent API，以及 legacy mirror 信令。
- `crates/core`：保留给 legacy mirror 的 ScreenCaptureKit、编码、几何、CGEvent 和租约。
- `web/index.html`：默认 Direct MJPEG + HTTP 控制；显式 mirror 时使用旧 WebRTC。
- `install.sh`、`scripts/`、`deploy/`：安装、WDA 设置和 LaunchAgent。
- `docs/`：接口、架构、验收和排障文档。

## 许可证

[MIT](LICENSE)
