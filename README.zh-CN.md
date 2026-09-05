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

Mac 上的一个守护进程在 USB 连接的 iPhone 上运行 WebDriverAgent（WDA），把手机交给三种使用者：

| 你是 | 用什么 | 从哪看 |
|---|---|---|
| 坐在浏览器前的人 | `http://<mac>:44321/phone`：实时画面、点按输入、流程录制 | [快速开始](#快速开始) |
| agent 或脚本 | `/agent/*` 下带 bearer 鉴权的 HTTP API | [Agent API](#agent-api) |
| Claude Code / Claude Desktop / 任何 MCP 客户端 | 随包的 `iphone-use-mcp`，20 个工具 | [MCP server](#mcp-server) |
| 要反复做同一件事的人 | 官方源里审阅过的 **flow**：一条命令，不经过模型 | [Flow 与官方源](#flow-与官方-flow-源) |

所有动作都发生在手机上。默认的 `direct` 后端不用 macOS 的 iPhone 镜像、屏幕录制、辅助功能、Mac 光标或前台窗口，并且 fail closed：WDA 不可用时控制请求直接报错，不会去动 Mac 上的任何东西。旧的镜像路径只在显式设置 `PHONE_REMOTE_BACKEND=mirror` 时启用，见[旧镜像后端](#旧镜像后端)。

> 现状：WDA 的元素树、文字、点按和截图能力各自有真机记录。浏览器整条链路的[真机验收矩阵](#真机验收边界)还没记完；构建通过或 `/agent/status` 健康都不能代替那份记录。

## 工作原理

```text
浏览器 <── GET /agent/mjpeg ── iphone-use daemon ── 127.0.0.1:9100 ──┐
浏览器 ── POST /control ─────> iphone-use daemon ── 127.0.0.1:8100 ──┤ iPhone 上的 WDA
Agent  ── /agent/* ──────────> iphone-use daemon ── 127.0.0.1:8100 ──┘
```

- `scripts/setup-wda.sh` 编译并签名 WDA，在手机上启动 XCUITest runner，用 `iproxy` 建两条固定的 loopback 中继：`8100` 控制，`9100` MJPEG 画面。daemon 只和 localhost 说话，后台进程手里不会攥着一个会变的手机 IP。USB 是唯一支持的路径，Wi-Fi 或 `socat` 属于手动实验。
- 浏览器从 `/agent/mjpeg` 拿实时画面（失败时退回 PNG 静帧），通过 `POST /control` 发输入，每条命令都返回成功或失败，不会把一个可能已经断掉的通道当成功。
- agent 用 `/agent/elements` 读文本形式的辅助功能树，用 `/agent/screenshot` 拿 PNG，用 `/agent/input` 做单步动作，或用 `/agent/actions` 跑一批带检查点的步骤。
- daemon 负责 WDA 的生命周期：空闲后释放手机、带退避地重建 WDA、把每个状态写进 `/agent/status`。

设计、生命周期、失败状态和安全边界见 **[`docs/direct-device-architecture.html`](docs/direct-device-architecture.html)**。

## 快速开始

### 前置条件

- macOS 15 或更高，装**完整 Xcode.app**（只有 Command Line Tools 不够）。在 Xcode → Settings → Accounts 登录并选一个开发团队；免费 Personal Team 能用，但 WDA 的描述文件要定期续。
- iPhone 开启**开发者模式**，通过 USB 与 Mac 配对并点过信任。
- 编译、启动、使用 WDA 期间手机保持**解锁、亮屏**。WDA 过不了 Face ID 和密码。
- `brew install libimobiledevice` 装 `iproxy`。
- 只有从源码构建才需要 Rust 工具链。

### 安装并接上第一台手机

```bash
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
```

安装器下载最新 GitHub Release，注册当前用户的 LaunchAgent（`PHONE_REMOTE_BACKEND=direct`），写入 loopback 的 WDA 地址，安装同版本的 agent skill，并把设置脚本放到 `~/.iphone-use/setup-wda.sh`。它不会证明你的团队、手机、runner 和中继能一起工作，那是下一步的事。手机连上、信任、解锁、亮屏后：

```bash
~/.iphone-use/setup-wda.sh doctor    # 解释当前的 USB / 信任 / DDI / WARP 阻塞项
~/.iphone-use/setup-wda.sh           # 编译、签名、安装、启动 WDA，拉起中继
~/.iphone-use/setup-wda.sh status
```

然后打开 **`http://<Mac局域网IP>:44321/setup`**。内置向导把 `/agent/status` 翻译成当前的阻塞项（USB、信任、开发者服务、WDA、外部主机），它不会改你的 VPN，也不会替你跑 setup。手机可驱动后进 **`/phone`**，输入 `install.sh` 打印的密码。

配对了多台 iPhone？两处固定同一个 classic UDID：

```bash
export PHONE_REMOTE_UDID=00008…
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
WDA_UDID="$PHONE_REMOTE_UDID" ~/.iphone-use/setup-wda.sh
```

### 把手机还给自己

WDA 运行期间会占着手机。想自己用手机，先暂停托管的 WDA，下次 agent 用之前再恢复：

```bash
~/.iphone-use/setup-wda.sh pause     # 禁用 launchd job，只停 PID 校验通过的进程
~/.iphone-use/setup-wda.sh resume
```

更省事的是网页控制栏里的 **交还** 按钮（或 `POST /agent/mode {"mode":"human"}`）：daemon 停掉 runner、在 Mac 上打开 iPhone 镜像，状态里 `human_handoff:true`，此时 agent 的输入请求一律 409 `phone_handed_to_human`，不会在你用着的时候把手机抢回去。用完点 **重新连接**（或 `{"mode":"agent"}`）还给 agent。

**人在外面想远程操作手机**：iPhone 镜像只能在这台 Mac 上开，所以远程走"远程进 Mac"这条路——把 Mac 和你的设备都放进 Tailscale，用 macOS 自带的屏幕共享连上来，先点"交还"，再在镜像窗口里操作。浏览器里直接看 WebRTC 画面、点画面控制手机（不经过 Mac 桌面）目前没有做；WDA 那条链路是给 agent 用的，延迟和锁屏要求都不适合人。

daemon 也可以自己做这件事：设置 `PHONE_REMOTE_IDLE_RELEASE_SECS`（比如 `300`），这么久没有 agent 活动、也没有人在看画面，它就停掉 runner。v0.6.3 起默认关闭，runner 常驻，下一次请求不用等重建。无论哪种情况，下一次 agent 请求或 `POST /agent/mode {"mode":"agent"}` 都会把 WDA 拉回来，手机锁了就解一下。

### 升级

daemon 每天检查一次 GitHub，在 `/agent/status` 里报 `version` / `latest` / `update_available`，网页会挂横幅。升级就是再跑一遍安装命令，安装器具体校验什么见[运维 → 升级](#升级-1)。

## 在浏览器里操作手机

`/phone` 显示手机端 MJPEG 画面，把你的点击、拖动、长按、滚动和输入（含中文）变成带 `ttl_ms` 上限、逐条确认的 `POST /control` 命令，不抢 Mac 焦点。**控件**面板列出辅助功能树，可以按精确标签点，不用对像素。

**流程**面板把你的操作录成可重放的 flow 文件：

- 只记录服务端确认成功的动作。优先保存精确的辅助功能标签；坐标手势标为易失。
- 输入的文字变成命名的运行参数，原文丢弃，不写进下载的 JSON。
- 每个动作后录制器比对元素树，能证明出现了新的唯一 identifier 或前台 app 变化时，插入一个可审阅的 `wait_for` 检查点；否则保留一段可见的短等待。它不会把任意标签或值抄进检查点，那里面可能有隐私。
- 审阅、排序、删除、填参数，然后下载合法的 flow v1 JSON 或执行一次。有动作没记下来的录制标为不完整草稿，不能执行。执行前必须填满参数，并勾选"不含不可逆操作"。
- **打开脚本**按和 CLI 相同的限制严格校验后，重新载入保存过的 flow；字面文字会被拒绝，必须是命名参数。

在这里录下来的东西，就是[官方源](#flow-与官方-flow-源)分发的东西。

## Agent API

完整参考：**[`docs/agent-api.html`](docs/agent-api.html)**。随包的 skill（[`skills/iphone-use/SKILL.md`](skills/iphone-use/SKILL.md)）教 agent 这套循环。

### 鉴权与请求头

| 请求头 | 何时 | 含义 |
|---|---|---|
| `Authorization: Bearer <token>` | 每个 `/agent/*` 调用 | 设了 `PHONE_REMOTE_AGENT_TOKEN` 就用它；否则回退到 daemon 密码。 |
| `X-Phone-Control: 1` | 每个改状态的 POST | 鉴权之上的 CSRF / 意图保护，不能代替鉴权。`/control`、`/agent/input`、`/agent/actions`、`/agent/mode`、`/agent/hold`、`/agent/owner` 和 `/agent/inbox` 的 POST 都要。网页和 MCP 客户端会自动加。 |
| `X-Phone-Owner: <会话名>` | 控制请求 | 为本会话认领手机（issue #72）。租约存活期间（每次请求刷新，`PHONE_REMOTE_OWNER_LEASE_SECS` 默认 300）其他会话和不带头的客户端收到 `409 phone_owned`，附 owner 名和剩余秒数。只读调用不受影响。`X-Phone-Owner-Takeover: 1` 强行接管并记日志。 |

### 端点

| 方法 | 路径 | 用途 |
|---|---|---|
| `GET` | `/agent/status` | 就绪与生命周期：`backend`、`device_state`、`drivable`、`wda_actionable`、`recovery_owner`、`setup_blocked_on` / `setup_phase` / `setup_message`、`hint`、viewer 计数、`instance`、`udid`、`owner` / `owner_lease_remaining_secs`、`hold_remaining_secs`、`version` / `latest`。 |
| `GET` | `/agent/screenshot` | 当前屏幕 PNG，来自手机。 |
| `GET` | `/agent/elements` | 扁平化的辅助功能树，带一次性 `snapshot` 令牌、`ax_stats` 可用性块，以及系统弹窗在场时的 `alert` 块。`?since=<snapshot>` 只返回 `delta`。WDA 缺失或繁忙 `503`，source 失败 `502`，不会用空数组伪装 `200`。 |
| `GET` | `/agent/mjpeg` | 鉴权后的实时 MJPEG。 |
| `POST` | `/agent/input` | 一个动作：点按、拖动、长按、滚动、文字、按键、`home` / `spotlight`、`launch_app`、`set_value`、`perform`、`alert`。`?return=delta` 同时返回动作后稳定下来的树变化。 |
| `POST` | `/agent/actions` | 最多 24 个 `action` / `wait_for` / `pause` 步骤，整批预校验，一把 WDA 锁，首个失败即停。返回 `completed`、`applied_actions`、`failed_step`、`outcome`、`retry_safe`。 |
| `POST` | `/agent/mode` | `{"mode":"agent"}` 重启已配置的 Direct 目标。不换后端，不换 UDID。 |
| `POST` | `/agent/hold` | `{"secs":N}`（0 清除，上限 14400）在人工暂停期间阻止空闲释放。释放已经开始时返回 `503 device_release_in_progress`。 |
| `POST` | `/agent/owner` | `{"release":true}` 提前归还 owner 租约。 |
| `GET` | `/agent/intents` | 语义意图注册表，见[语义意图](#语义意图手机端快捷指令)。 |
| `POST` | `/agent/intent` | 派发一个已注册的 verb，结果落到 `/agent/inbox`。 |
| `GET` / `POST` | `/agent/inbox`、`/agent/inbox/drain` | 查看 / 追加 / 原子清空快捷指令结果队列。 |
| `POST` | `/control` | 浏览器输入，cookie 鉴权，`ttl_ms` 必填且在 1–2500 ms 之间。 |

### agent 必须遵守的语义

- **只看 `drivable:true`**（以及 `backend:"direct"`、`wda_actionable:true`）。`device_state` 取值：`ready`、`locked`、`blocked`、`offline`、`releasing`、`released`、`reconnecting`。`phone_target`、`mirror_state`、`human_active` 是旧镜像字段。
- **至多送达一次。** 派发前过期返回 `408 not_sent`，`retry_safe:true`。派发后传输失败 `502`，派发后超时 `504`，两者都是 `outcome_unknown`、`retry_safe:false`：先看屏幕，再决定要不要再来一次。
- **目标绑定快照。** 元素索引只在同一次 `/agent/elements` 返回的 `snapshot` 下有效，树变了就 `409 stale_element_snapshot`。精确标签点按在零匹配或多匹配时都不动作。脚本里存标签、identifier、locator，不存索引和快照令牌。
- **元素级动作。** `set_value` 直接写字段（先清再填），带 `element` 的 `scroll` 把手势限制在该元素内，`perform` 调用命名能力（`increment`、`decrement`、`adjust`、`toggle`、`menu`、`double_tap`、`two_finger_tap`、`scroll_to_visible`、`pinch`、`rotate`、`force_press`）。`PHONE_REMOTE_ELEMENTS_AFFORDANCES=1` 让树上标出每行支持哪些动作。
- **系统弹窗是另一层。** 点它的按钮会被确认但不生效。用 `{"type":"alert","button":"…"}` 或 `{"action":"accept"|"dismiss"}`。
- **`/agent/actions`** 只要有动作已落地，就不会把整批重放标成安全。`tap_locator` 与 `wait_for` 用同一套精确的 label / identifier / kind / value / 状态字段，要求当前唯一命中。

```bash
HOST=http://<Mac局域网IP>:44321; AUTH="Authorization: Bearer $TOKEN"
MUTATION="X-Phone-Control: 1"; OWNER="X-Phone-Owner: my-script"
curl -s -H "$AUTH" "$HOST/agent/status"
curl -s -H "$AUTH" "$HOST/agent/screenshot" -o screen.png
curl -s -H "$AUTH" -H "$MUTATION" -H "$OWNER" -X POST "$HOST/agent/input" -d '{"type":"tap","x":0.5,"y":0.3}'
curl -s -H "$AUTH" -H "$MUTATION" -H "$OWNER" -X POST "$HOST/agent/input" -d '{"type":"text","text":"你好"}'
curl -s -H "$AUTH" -H "$MUTATION" -H "$OWNER" -X POST "$HOST/agent/actions" \
  -d '{"steps":[{"kind":"action","action":{"type":"shortcut","name":"home"}},{"kind":"wait_for","expect":{"present":[{"label":"搜索"}]},"timeout_ms":3000}]}'
```

### 语义意图（手机端快捷指令）

界面够不着、或点起来太慢的事（电量、健康样本、带原生确认的发消息），走一组精选的 **verb**，由一个桥接快捷指令执行。daemon 通过 WDA 在手机上打开 `shortcuts://run-shortcut`，不经过 Spotlight 和剪贴板；快捷指令把结果 POST 回 `/agent/inbox`。

```bash
python3 deploy/make-bridge-shortcut.py --token "$PHONE_REMOTE_AGENT_TOKEN"
open "iU Bridge.shortcut"     # 接受导入；iCloud 会同步到手机
```

verb 定义在 `~/.iphone-use/intents-registry.json`（从 [`deploy/intents-registry.example.json`](deploy/intents-registry.example.json) 起步）。快捷指令名必须等于注册表的 `bridge.name`，bearer token 放在快捷指令自己的请求头里。`--self-test` 检查 plist 里那些会静默失败的部分。每个 verb 首次使用要在手机上点一次授权，调用期间快捷指令会到前台。

**回传路径要求手机能连到 daemon**（issue #59）。默认的 `PHONE_REMOTE_HOST=127.0.0.1` 能派发 verb 但收不到回答，所以意图通道默认是关着的，要用得先从下面选一个回传路径：

| 回传路径 | 做法 | 代价 |
|---|---|---|
| 绑局域网 | `PHONE_REMOTE_HOST=0.0.0.0`，同时设密码**和** `PHONE_REMOTE_AGENT_TOKEN` | 最简单；daemon 的鉴权面暴露给整个局域网。 |
| USB 反向隧道 | 把手机侧端口转回 Mac 的 loopback 监听 | 不暴露局域网；多几样要维护的东西。 |

只发不收的 verb 在纯 loopback 下能用。不可信网络上别绑 `0.0.0.0`：WDA 自己的 `8100` / `9100` 没有任何鉴权。

## MCP server

[`iphone-use-mcp`](crates/mcp/README.md) 随安装的 app 一起交付（`~/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp`），每个 release 也单独发一份带校验的压缩包。它通过 stdio 说 MCP，并自动给 daemon 请求加 `X-Phone-Control` 和 `X-Phone-Owner`（`PHONE_REMOTE_OWNER`，默认 `mcp-<pid>`）。

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

| 分组 | 工具 |
|---|---|
| 看 | `phone_status`、`phone_screenshot`、`phone_elements`（带 `registry` 块，列出当前屏幕上这个 app 已安装的 flow） |
| 动 | `phone_tap`、`phone_tap_element`（绑定快照）、`phone_tap_label`（精确标签唯一）、`phone_scroll`、`phone_type`（中文无损）、`phone_key`、`phone_shortcut`（`home` / `spotlight`） |
| 批 | `phone_run_steps`：最多 24 步，含 `tap_locator`、`launch_app`、`picker`、`alert`、长按 / 滑动 / 拖动、`wait_for` |
| 生命周期 | `phone_reconnect`（重启已配置的 Direct 目标，不换 UDID）、`phone_hold`、`phone_release_owner` |
| Flow | `phone_flow_list`、`phone_flow_info`、`phone_flow_run`、`phone_flow_update`、`phone_flow_publish`、`phone_flow_report` |

收起键盘、卸载、目标配置仍只走 HTTP。完整 schema 见 [`crates/mcp/README.md`](crates/mcp/README.md)。

## Flow 与官方 flow 源

**flow** 是一份严格的 JSON（`version: 1`），步骤和 `phone_run_steps` 一样带检查点，外加命名的字符串参数。`iphone-use-mcp` 二进制能校验并运行它，全程不经过模型：

```bash
MCP="$HOME/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp"
"$MCP" flow validate examples/flows/search-spotlight.json          # 离线
PHONE_REMOTE_TOKEN=… "$MCP" flow run examples/flows/search-spotlight.json --input 'query=咖啡'
```

**官方源** [`leeguooooo/iphone-use-flows`](https://github.com/leeguooooo/iphone-use-flows) 是唯一支持的源，把 flow 做成可安装的目录，按 app 分组、经过审阅，和 chrome-use 分发 site 包一个思路：

```bash
"$MCP" flow update                        # 镜像到 ~/.iphone-use/flows：sha256 + 严格校验，0600
"$MCP" flow list --category health        # id · risk · verified · inputs · name
"$MCP" flow info health/export-all-zh-cn  # 元数据和步骤模板
PHONE_REMOTE_TOKEN=… "$MCP" flow run health/export-all-zh-cn
"$MCP" flow add my.json --as myapp/daily  # 自己的 flow，update 不会删
"$MCP" flow publish my.json --as myapp/daily --alias 某App --note "iPhone 17 Pro Max, iOS 26"   # 用 gh 开 PR
"$MCP" flow report health/export-all --result @run.json --note "资料按钮改名了"                  # 提 flow-broken issue
```

flow 上的源元数据都是可选的：`app`（bundle id）、`category`、`risk`（`read_only` · `navigation` · `side_effect`，`side_effect` 没有 `--confirm` / `confirm=true` 拒绝运行）、`locale`（标签和语言绑定）、`tags`、`verified_on`（证明过这份文件的真机记录）。文件是纯 JSON，安装源不执行任何代码；任何一个校验或哈希失败都会中止整次更新，本地目录原样不动。

格式里定死的规则：`--input KEY=VALUE` 只在本次运行解析，不写回文件；flow 在第一个失败步骤停下，不自动重试；命令行参数会留在 shell 历史里，所以参数不能装凭据、验证码或隐私内容，发送 / 发布 / 支付 / 删除类动作必须声明 `side_effect`。

app 更新不会让源悄悄失效：每条 flow 记着它在哪个 app（系统 app 则是 iOS）版本上验过，CLI 读出手机上实际装的版本（`flow apps`），每次列表都给出 `compat` 结论：`verified`、`untested-newer`、`incompatible`、`broken`、`needs-verification`、`draft`、`unknown`。`flow run` 对 broken / incompatible 的 flow 不带 `--force` 就拒跑。夜间 canary（`scripts/flow-reverify.py`）在真机上重跑已验证的只读 flow，刷新 `verified_on`，失败的打标记。

agent 不靠记性去查源，而是被推着走：`phone_elements` 直接列出屏幕上这个 app 已安装的 flow；`phone_run_steps` 成功跑完 3 步以上会提示把这段存成 flow；`phone_flow_run` 失败时保留现场，`phone_flow_report` 只需补一句说明。格式背后的调研见 [`docs/scripted-flows-research.html`](docs/scripted-flows-research.html)。

## 运维

### 生命周期与恢复

`/agent/status` 是唯一事实来源。`recovery_owner` 在托管 loopback WDA 下是 `daemon`，首次接入尚未持久化目标时是 `unconfigured`，不托管的端点是 `external`。锁屏导致失败后，daemon 重建 WDA 的间隔从 30 秒退避到 15 分钟，不会反复催密码；其他失败从 5 秒退避到 5 分钟；一次成功恢复清零两种退避。交互式 setup 最多等 5 分钟解锁。`POST /agent/mode {"mode":"agent"}`（MCP 里是 `phone_reconnect`）只重启一次已配置的目标，不要循环调；先读 `hint` 和 `setup_blocked_on`（`warp|proxy|usb|trust|ddi|account`）。

### 升级

```bash
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
```

安装器把 release tag 解析成一个确定的 commit，helper 和 skill 都钉在上面；daemon app 取对应 Release asset 并校验 SHA-256；先把 skill 安装到 `~/.agents/skills/iphone-use`、逐字节校验落盘内容和 Claude Code 发现链接，再替换 daemon。skill 失败中止升级；之后 daemon 失败则恢复旧 skill。`IPHONE_USE_SKIP_SKILL=1` 保留现有 skill，这是降级安装，安装器不保证新 daemon 和旧 skill 兼容。迁移按证据判断：旧 plist 有有效的 loopback `PHONE_REMOTE_WDA_URL` 就迁到 Direct，完全没有 WDA 配置的旧安装留在 Mirror，显式配置的后端不动。`PHONE_REMOTE_NO_UPDATE_CHECK=1` 关掉每日检查。

安装时的签名跟后端走：Direct 保留有效的现有签名，无效时用不动 keychain 的 ad-hoc 签名（Direct 不需要 TCC 身份）；Mirror 用稳定的本地 `iPhoneUse Local Signing` 身份，让 TCC 授权跨升级保留，退回 ad-hoc 前会警告。

### 配置

| 环境变量 | 默认 | 用途 |
|---|---|---|
| `PHONE_REMOTE_BACKEND` | `direct` | `direct` = WDA 输入 + 手机端 MJPEG；`mirror` = 旧的 ScreenCaptureKit + CGEvent 路径。 |
| `PHONE_REMOTE_HOST` / `PHONE_REMOTE_PORT` | `127.0.0.1` / `44321` | 监听地址和端口（局域网用 `0.0.0.0`，此时必须设密码）。 |
| `PHONE_REMOTE_PASSWORD` | 无 | 浏览器登录密码；没设 agent token 时兼作 bearer。 |
| `PHONE_REMOTE_AGENT_TOKEN` | 无 | 专用 agent bearer。设了以后是**唯一**接受的 bearer。 |
| `PHONE_REMOTE_UDID` | 安装器识别并持久化 | 托管 WDA 和破坏性命令使用的 canonical iPhone。请求不能临时换机，要改就改部署并重启。setup 时传同值 `WDA_UDID`。 |
| `PHONE_REMOTE_WDA_URL` / `PHONE_REMOTE_WDA_MJPEG_URL` | `http://127.0.0.1:8100` / `:9100` | WDA 控制和 MJPEG 的 loopback。不可达时 Direct 直接失败。 |
| `PHONE_REMOTE_WDA_MANAGED` | loopback 端点默认开 | daemon 是否负责 WDA supervisor / 中继的生命周期。 |
| `PHONE_REMOTE_IDLE_RELEASE_SECS` | `0` | 空闲多少秒后停 WDA（v0.6.3 之前默认 `300`）；`0` 表示常驻，重连不用重建。 |
| `PHONE_REMOTE_OWNER_LEASE_SECS` | `300` | `X-Phone-Owner` 租约在没有请求刷新时的存活时间。 |
| `WDA_RUNNER_ICON` | `auto` | runner 的桌面图标：`auto` 用 app 图标，`none` 用 WDA 占位图，或给 `.png` / `.icns` 路径。失败只警告。 |
| `PHONE_REMOTE_WDA_SNAPSHOT_MAX_DEPTH` | WDA 默认 50 | 限制辅助功能快照深度（树特别大的 app 试 `20`–`30`，issue #44）。 |
| `PHONE_REMOTE_WDA_SNAPSHOT_TIMEOUT_S` | WDA 默认 15 | 限制快照解析时间，让一次过大的读取失败而不是卡死 runner。 |
| `PHONE_REMOTE_ELEMENTS_AFFORDANCES` | 关 | `1` 给 `/agent/elements` 的行加稀疏的 `actions`、`selected`、`min` / `max`。 |
| `PHONE_REMOTE_ELEMENTS_TRAITS` | 关 | `1` 再输出原始的辅助功能 trait 名。 |
| `PHONE_REMOTE_NO_UPDATE_CHECK` | 关 | 跳过每日 release 检查。 |
| `PHONE_REMOTE_CF_TURN_*`、`PHONE_REMOTE_TURN_*`、`PHONE_REMOTE_AUTO_RESUME` | — | 仅旧镜像 / WebRTC 使用。 |

## 安全

daemon 把手机的实时控制放到了网络上，它的 URL 和密码要当凭据对待。

- 密码 / cookie / bearer 只保护 `44321`。**手机上 WDA 自己的 `8100` 和 `9100` 没有鉴权**，USB `iproxy` 中继也不会加，手机所在 Wi-Fi 里的另一台机器能直接连过去。Direct 只在可信、隔离的网络里用；走 USB 时关掉手机 Wi-Fi 就没有这层暴露。
- 真正带鉴权的设备传输属于 Phase 2（companion app 或受控隧道）。在此之前，daemon 的登录不等于 WDA 的保护。
- 远程访问时把 `44321` 放在你自己管理的 HTTPS 隧道后面；daemon 只提供明文 HTTP，识别 `X-Forwarded-Proto`，session cookie 是 `HttpOnly` + `SameSite=Lax`。
- owner 租约（`X-Phone-Owner`）是协作会话之间的协调机制，不是安全边界。
- 开放访问期间不要停在支付、私聊或 2FA 画面。不用时停掉 LaunchAgent。

### WARP / VPN

WARP 一类 VPN 会切断 WDA 依赖的 CoreDevice 隧道。`setup-wda.sh doctor` 能检测到，`/agent/status` 会报 `device_state:"blocked"`、`setup_blocked_on:"warp"`；两者都不会去改你的 VPN，那是操作者的决定，公司电脑需要管理员配 split tunnel。

WARP 同样会打挂 **iPhone 镜像本身**，哪怕本项目一行都没跑（issue #17，在 macOS 26 和 27.0 beta 上各自复现）：镜像走接力（Continuity），VPN 会拖垮它。往这里提 bug 之前先自查：停掉我们的 LaunchAgent（`launchctl bootout gui/$(id -u)/com.leeguoo.iphone-use` 和 `.wda` 那个 job），退出镜像，`warp-cli disconnect`，再开镜像。能连上就说明 daemon 从头到尾没参与。Zero Trust 的 *Always On* 策略会自动把 WARP 连回去，只有管理员配的排除规则能长期解决。

## 旧镜像后端

`PHONE_REMOTE_BACKEND=mirror` 用 ScreenCaptureKit 抓 iPhone 镜像窗口，VideoToolbox 编 H.264，WebRTC 传画面，CGEvent 注入输入。它需要镜像已连接、屏幕录制和辅助功能权限、已登录的 Aqua 会话，以及一个能置前的镜像窗口。`assets/` 里的架构图描述的是这个后端，不是默认路径。

**"iU Bridge" 快捷指令实验**（`shortcuts/`）属于这个后端：它从 Mac 侧打开 Spotlight、灌剪贴板和键盘事件。Direct 下的替代是[语义意图通道](#语义意图手机端快捷指令)。App Switcher、控制中心和任意 Mac 键码在 Direct 下没有手机端实现之前不支持。

## 开发

```bash
cargo build --release --bin iphone-use --bin iphone-use-mcp
./scripts/make-app.sh                  # → ./iPhoneUse.app
./install.sh ./iPhoneUse.app           # 签名、安装、写 LaunchAgent（使用工作树里的 skill）

# 或者不安装直接跑 daemon
PHONE_REMOTE_BACKEND=direct PHONE_REMOTE_WDA_URL=http://127.0.0.1:8100 \
PHONE_REMOTE_WDA_MJPEG_URL=http://127.0.0.1:9100 \
PHONE_REMOTE_HOST=0.0.0.0 PHONE_REMOTE_PASSWORD=secret ./target/release/iphone-use serve
```

| 路径 | 内容 |
|---|---|
| `crates/server` | daemon：WDA 控制、MJPEG 代理、浏览器 `/control`、agent API、旧镜像信令 |
| `crates/mcp` | `iphone-use-mcp`：MCP server、flow 运行器、源客户端、`flow publish` / `report` |
| `crates/core` | ScreenCaptureKit、编码、几何、CGEvent，仅旧镜像使用 |
| `web/index.html` | 浏览器客户端（默认 MJPEG + `/control`，镜像下用 WebRTC） |
| `skills/iphone-use` | 安装器随包交付的 agent skill |
| `scripts/`、`deploy/`、`install.sh` | WDA 设置、打包、LaunchAgent、桥接快捷指令生成器 |
| `docs/` | 架构、agent API 参考、WDA 设置、flow 调研 |

### 路线

- [x] Direct/WDA 元素树控制、Unicode 文字、标签点按、手机端截图（iPhone 17 / iOS 27 上有组件级记录，见 [`docs/wda-setup.html`](docs/wda-setup.html)）。
- [x] MCP server；CI 出 release 二进制，一行命令安装。
- [x] 确定性 flow、官方 flow 源，以及 publish / report 两条回路。
- [ ] 记完下面的浏览器整链路真机验收矩阵。
- [ ] 让首次接机、签名续期、休眠 / 重连恢复、多设备选择在产品界面里看得懂。
- [ ] 逐条重验 Direct 下的每个命令，不继承镜像时代的能力名称。
- [ ] Phase 2 带鉴权的设备传输（companion app 或受控隧道）。
- [ ] 一段 agent 操作手机的演示。

### 真机验收边界

下面各项都在真实 iPhone 上观察到，浏览器 Direct 默认路径才算验收：

1. Mac 不给屏幕录制 / 辅助功能权限、不开镜像，安装并跑 WDA setup，Direct 持续在线。
2. `/agent/status` 对目标 UDID 报 `backend:"direct"`、`wda:true`、`wda_actionable:true`、`drivable:true`。
3. 另一台设备打开 `/phone` 画面持续更新；停掉 9100 中继后界面报降级 / 离线，而不是装作正常。
4. `/control` 的点按、拖动、长按、滚动、ASCII 和中文各执行一次，都有确认且只落地一次。
5. `/agent/elements`、`/agent/screenshot`、`/agent/input` 走 bearer 鉴权正常，WDA 故障时也符合文档；任何命令都不动 Mac 光标。
6. 观察到 `releasing → released → reconnecting → ready`，并覆盖锁屏 / 解锁、USB 重连、Mac 重启、WDA 重签重装，多设备 Mac 上目标不串。
7. 在隔离网络里记录手机 IP 是否暴露无鉴权的 `8100/9100`。

## 反馈

碰到问题请[开 issue](https://github.com/leeguooooo/iphone-use/issues)。也欢迎 AI agent 来提：随包的 skill 让它在 API 误导自己时（经用户同意）提结构化 issue，flow 的问题则提到[官方源](https://github.com/leeguooooo/iphone-use-flows/issues)。

## 许可证

[MIT](LICENSE)
