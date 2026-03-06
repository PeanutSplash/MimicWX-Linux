# MimicWX-Linux 🐧

**零风险微信自动化框架** — 基于 AT-SPI2 无障碍接口 + X11 XTEST 输入注入 + SQLCipher 数据库解密

> Zero-risk WeChat automation framework for Linux via AT-SPI2 accessibility + X11 XTEST input injection + SQLCipher database decryption

---

## ✨ 特性

- 🔍 **数据库消息检测** — SQLCipher 解密 WCDB + fanotify WAL 实时监听，亚秒级延迟，支持文本/图片/语音/视频/链接等 13+ 种消息类型解析
- ⌨️ **X11 原生输入注入** — XTEST 扩展注入键鼠事件 + X11 Selection 协议直接操作剪贴板（零外部进程依赖），原生窗口管理
- 🔑 **GDB 自动密钥提取** — 在 `setCipherKey` 偏移处设断点，扫码登录后自动从寄存器捕获 32 字节 AES 密钥
- 💬 **独立聊天窗口** — 借鉴 [wxauto](https://github.com/cluic/wxauto) 的 ChatWnd 设计，支持多窗口并行收发 + NodeHandle 缓存自动失效重建
- 🔌 **REST + JSON-RPC over WebSocket** — 完整 HTTP API + WebSocket 双向通信（JSON-RPC 2.0 请求/响应 + 实时事件推送），CORS 全开放，可对接 Yunzai 等机器人框架
- 📸 **截图 + 终端二维码** — X11 截屏微信窗口（多窗口自动拼接），登录时终端自动渲染二维码，无需 VNC 即可扫码
- 🐳 **Docker 一键部署** — 多阶段构建，默认 headless 运行（Xvfb + openbox），`docker compose up -d` 即可完成部署和登录
- 🔒 **Token 认证** — 支持 Bearer Token 认证保护 API 安全
- 🖥️ **交互式控制台** — 支持 `/restart`、`/stop`、`/status`、`/refresh`、`/help` 命令，方向键切换历史
- 💡 **自动弹性** — AT-SPI2 心跳自动重连、联系人定时刷新、优雅重启/关闭
- 🔄 **运行时状态机** — 显式 RuntimeState（Booting → DesktopReady → WeChatReady → LoginWaiting → KeyReady → DbReady → Serving），异常进入 Degraded 而非 panic
- 📊 **输入层可观测** — 队列深度、命令耗时、剪贴板失败、焦点丢失等关键指标实时暴露

---

## 🏗️ 系统架构

```
┌─ Docker 容器 (Ubuntu 22.04) ──────────────────────────────────────────────┐
│                                                                           │
│  ┌─ 显示层 ───────────────────────────────────────────────────────────────┐│
│  │  默认: Xvfb (虚拟显示 :1) + openbox (轻量 WM)  ← headless 模式      ││
│  │  调试: MIMICWX_DEBUG=1 → TigerVNC + noVNC (浏览器远程桌面)           ││
│  │  WeChat Linux 版                                                      ││
│  └───────────────────────────────────────────────────────────────────────┘│
│                                                                           │
│  ┌─ MimicWX 核心 (Rust) ────────────────────────────────────────────────┐ │
│  │                                                                       │ │
│  │  ┌── 运行时层 ──────────────────────────────────────────────────┐     │ │
│  │  │  runtime.rs: RuntimeState 状态机驱动启动 + 广播状态变更       │     │ │
│  │  │  ports.rs:   Ports/Adapters 抽象 (KeyProvider / MessageSource │     │ │
│  │  │              / MessageSender / SessionLocator)                │     │ │
│  │  └──────────────────────────────────────────────────────────────┘     │ │
│  │                                                                       │ │
│  │  ┌── 消息检测层 ────────────────────────────────────────────────┐     │ │
│  │  │  db.rs:    SQLCipher 解密 → fanotify WAL 监听 → 增量消息拉取 │     │ │
│  │  │  atspi.rs: D-Bus → AT-SPI2 Registry → 节点遍历/属性读取      │     │ │
│  │  └──────────────────────────────────────────────────────────────┘     │ │
│  │                                                                       │ │
│  │  ┌── 输入控制层 ────────────────────────────────────────────────┐     │ │
│  │  │  input.rs:       X11 XTEST 键鼠注入 + X11 Selection 剪贴板   │     │ │
│  │  │  node_handle.rs: AT-SPI 节点指纹 + 缓存失效自动重定位        │     │ │
│  │  └──────────────────────────────────────────────────────────────┘     │ │
│  │                                                                       │ │
│  │  ┌── 业务逻辑层 ────────────────────────────────────────────────┐     │ │
│  │  │  wechat.rs:  会话管理 / 消息发送 / 控件查找 / 状态检测        │     │ │
│  │  │  chatwnd.rs: 独立聊天窗口管理 (多窗口并行)                    │     │ │
│  │  └──────────────────────────────────────────────────────────────┘     │ │
│  │                                                                       │ │
│  │  ┌── API 层 ────────────────────────────────────────────────────┐     │ │
│  │  │  api.rs:    axum HTTP + WebSocket (JSON-RPC 2.0 + 心跳保活)   │     │ │
│  │  │  events.rs: WxEvent 类型化事件 (Message/Sent/StatusChange)    │     │ │
│  │  │  main.rs:   状态机驱动启动 / 配置 / 消息循环 / 交互式控制台    │     │ │
│  │  └──────────────────────────────────────────────────────────────┘     │ │
│  └───────────────────────────────────────────────────────────────────────┘ │
│                                                                           │
│  ┌─ 辅助脚本 ────────────────────────────────────────────────────────────┐ │
│  │  start.sh:       容器启动编排 (D-Bus → X11 → AT-SPI2 → 微信 → 服务) │ │
│  │  extract_key.py: GDB Python 脚本 — 自动提取 WCDB 加密密钥          │ │
│  └──────────────────────────────────────────────────────────────────────┘ │
└───────────────────────────────────────────────────────────────────────────┘

┌─ 外部对接 ────────────────────────────────────────────────────────────────┐
│  adapter/MimicWX.js: Yunzai-Bot 适配器 (REST + WebSocket)                │
└───────────────────────────────────────────────────────────────────────────┘
```

---

## 📁 项目结构

```
MimicWX-Linux/
├── src/                        # Rust 源代码
│   ├── main.rs                 # 入口: 状态机驱动启动、配置加载、消息循环
│   ├── runtime.rs              # 运行时状态机 (RuntimeState 枚举 + RuntimeManager)
│   ├── ports.rs                # Ports/Adapters 抽象 (trait 定义 + 适配器实现)
│   ├── events.rs               # 类型化事件 (WxEvent 枚举 + JSON-RPC 序列化)
│   ├── atspi.rs                # AT-SPI2 底层原语 (D-Bus 通信、节点遍历)
│   ├── input.rs                # X11 XTEST 输入引擎 (键鼠注入、窗口管理)
│   ├── node_handle.rs          # AT-SPI 节点句柄 (指纹匹配 + 缓存失效重定位)
│   ├── wechat.rs               # 微信业务逻辑 (会话管理、消息发送/验证)
│   ├── chatwnd.rs              # 独立聊天窗口 (ChatWnd 模式)
│   ├── db.rs                   # 数据库监听 (SQLCipher + fanotify WAL)
│   └── api.rs                  # HTTP/WebSocket API (axum + JSON-RPC 2.0)
├── docker/
│   ├── start.sh                # 容器启动脚本 (headless/debug 双模式)
│   ├── extract_key.py          # GDB 密钥提取脚本
│   └── dbus-mimicwx.conf       # D-Bus 配置 (允许 eavesdrop)
├── adapter/
│   └── MimicWX.js              # Yunzai-Bot 适配器
├── Cargo.toml                  # Rust 依赖 & 构建配置
├── Dockerfile                  # 多阶段构建 (builder + runtime)
├── docker-compose.yml          # 编排配置
└── config.toml                 # 运行时配置文件
```

---

## 📦 核心模块详解

### `runtime.rs` — 运行时状态机

显式定义系统生命周期，每个阶段有明确的前置条件和退出条件：

| 状态 | 说明 |
|------|------|
| `Booting` | 系统服务启动中 |
| `DesktopReady` | X11 + AT-SPI2 连接就绪 |
| `WeChatReady` | 微信进程已启动，窗口可见 |
| `LoginWaiting` | 等待扫码登录 |
| `KeyReady` | GDB 密钥提取成功 |
| `DbReady` | DbManager 初始化完成，联系人已加载 |
| `Serving` | 全功能服务中 |
| `Degraded(reason)` | 部分功能不可用（附原因） |

状态变更通过 broadcast channel 通知 API 层，`/status` 返回精确的 RuntimeState。

### `ports.rs` — Ports/Adapters 抽象

定义业务边界 trait，将基础设施与业务逻辑解耦：

| Port (trait) | Adapter (实现) | 职责 |
|-------------|---------------|------|
| `KeyProvider` | `GdbFileKeyProvider` | 读取 GDB 提取的密钥文件 |
| `MessageSource` | `WcdbMessageSource` (db.rs) | 数据库增量消息 + fanotify 监听 |
| `MessageSender` | `ActorPort` | AT-SPI + X11 发送消息/图片 |
| `SessionLocator` | `ActorPort` | 会话切换 / 监听管理 |

### `events.rs` — 类型化事件

内部事件统一为 `WxEvent` 枚举，序列化为 JSON-RPC 2.0 通知推送：

| 事件类型 | 说明 |
|---------|------|
| `Message(DbMessage)` | 数据库新消息 |
| `Sent { to, text, verified }` | 发送结果确认 |
| `StatusChange { from, to }` | 运行时状态变化 |
| `Control { cmd }` | 控制命令 |

### `node_handle.rs` — AT-SPI 节点句柄

统一的 NodeHandle 机制，解决 AT-SPI 节点引用散落各模块的问题：

| 能力 | 说明 |
|------|------|
| **指纹匹配** | 通过 role + name pattern + 祖先路径定位节点 |
| **缓存失效** | 自动检测节点失效（bbox 校验），透明重搜 |
| **统一接口** | `resolve()` 一个方法搞定获取/重定位/失败检测 |
| **搜索限制** | DFS 最多 600 节点，防止遍历失控 |

### `atspi.rs` — AT-SPI2 底层原语

通过 `zbus` 连接 AT-SPI2 D-Bus，封装节点遍历和属性读取：

| 能力 | 说明 |
|------|------|
| **多策略连接** | `org.a11y.Bus` → `AT_SPI_BUS_ADDRESS` 环境变量 → `~/.cache/at-spi/` socket 扫描 |
| **运行时重连** | Registry 持续返回 0 子节点时自动重新发现 AT-SPI2 bus |
| **节点操作** | `child_count` / `child_at` / `name` / `role` / `bbox` / `text` / `parent` / `get_states` |
| **搜索原语** | BFS 广度搜索 + DFS 深度搜索，支持 role/name 过滤 |
| **超时保护** | 所有 D-Bus 调用带 500ms 超时 |

### `input.rs` — X11 XTEST 输入引擎

通过 `x11rb` 使用 XTEST 扩展注入输入事件：

| 能力 | 说明 |
|------|------|
| **键盘** | 单键按下 / 组合键 (`Ctrl+V`, `Ctrl+A` 等) / ASCII 逐字输入 |
| **中文输入** | X11 Selection 协议直接设置剪贴板 → `Ctrl+V` 粘贴 (零外部进程) |
| **图片发送** | `xclip -selection clipboard -t image/png` → `Ctrl+V` 粘贴 |
| **鼠标** | 移动 / 单击 / 双击 / 右键 / 滚轮 |
| **窗口管理** | X11 原生 `_NET_ACTIVE_WINDOW` 激活 / `_NET_CLOSE_WINDOW` 关闭 (替代 xdotool) |
| **窗口截图** | X11 `GetImage` 逐窗口截取微信窗口，多窗口自动水平拼接，输出 PNG |
| **二维码检测** | `rqrr` 灰度识别 QR → `qrcode` 终端 Unicode 半块字符渲染 (▀▄█) |
| **可观测指标** | InputMetrics 实时跟踪队列深度、命令耗时、失败计数 |

### `db.rs` — 数据库监听

SQLCipher 解密微信 WCDB 数据库 + fanotify 实时监听：

| 能力 | 说明 |
|------|------|
| **SQLCipher 解密** | `rusqlite` + `bundled-sqlcipher-vendored-openssl`，使用 GDB 提取的密钥 |
| **持久连接池** | 多个 `message_N.db` 保持长连接，避免重复解密握手 |
| **WAL 监听** | `fanotify` + PID 过滤 (只监听微信进程写入)，无需防抖 |
| **增量消息** | 每个消息表维护 `last_local_id` 高水位标记 |
| **联系人缓存** | 从 `contact.db` + `group_contact.db` 加载联系人/群成员 |
| **消息解析** | 支持文本/图片/语音/视频/表情/名片/链接/小程序/文件/转账/红包/系统消息 |
| **WCDB 兼容** | Zstd BLOB 解压 + TEXT/BLOB 自适应读取 |
| **发送验证** | 订阅自发消息广播，事件驱动验证发送结果 |

### `wechat.rs` — 微信业务逻辑

基于 AT-SPI2 的微信 UI 自动化：

| 能力 | 说明 |
|------|------|
| **状态检测** | 通过 `[tool bar] "导航"` 判断登录状态 (未运行/等待扫码/已登录) |
| **控件查找** | 导航栏 / split pane / 会话列表 / 消息列表 / 输入框 |
| **会话管理** | 列表获取 / 精确匹配优先切换 / 新消息检查 / Ctrl+F 搜索回退 |
| **消息发送** | 公共方法提取 → 切换会话 → 粘贴文本 → Enter → DB 验证 |
| **图片发送** | 优先独立窗口，回退主窗口 |
| **独立窗口** | 弹出 (`add_listen`) / 关闭 (`remove_listen`) / 存活检测 |

### `chatwnd.rs` — 独立聊天窗口

每个独立弹出的聊天窗口拥有独立的 AT-SPI2 节点：

| 能力 | 说明 |
|------|------|
| **窗口管理** | 创建 / 存活检查 / 销毁 |
| **NodeHandle 集成** | 输入框/消息列表通过 NodeHandle 自动管理失效重建 |
| **消息发送** | 激活窗口 → 发送文本/图片 → 验证 |

### `api.rs` — HTTP + WebSocket API

基于 `axum` 的 REST API + JSON-RPC over WebSocket：

| 端点 | 方法 | 说明 |
|------|------|------|
| `/status` | GET | RuntimeState + InputMetrics + DB/联系人/运行时间 (免认证) |
| `/screenshot` | GET | X11 屏幕截图 PNG (免认证, 用于扫码登录) |
| `/contacts` | GET | 联系人列表 (数据库) |
| `/sessions` | GET | 会话列表 (优先数据库) |
| `/messages/new` | GET | 新消息 (数据库增量) |
| `/send` | POST | 发送文本消息 |
| `/send_image` | POST | 发送图片 (base64) |
| `/chat` | POST | 切换聊天目标 |
| `/listen` | POST | 添加/查看监听目标 |
| `/listen` | DELETE | 移除监听目标 |
| `/command` | POST | 通用命令执行 (微信互通) |
| `/ws` | GET | WebSocket (JSON-RPC 2.0 双向通信 + 事件推送) |
| `/debug/tree` | GET | AT-SPI2 控件树 (调试) |
| `/debug/session_tree` | GET | 会话容器树 (调试) |

> 认证方式: `Header "Authorization: Bearer <token>"` 或 `Query "?token=<token>"`

**WebSocket JSON-RPC 2.0 支持：**

适配器只需一个 WS 连接即可完成所有操作（发送命令 + 接收事件）：

```json
// 请求 (客户端 → 服务端)
{"jsonrpc": "2.0", "method": "send", "params": {"to": "...", "text": "..."}, "id": 1}

// 响应 (服务端 → 客户端)
{"jsonrpc": "2.0", "result": {"sent": true, "verified": true}, "id": 1}

// 事件推送 (服务端 → 客户端, 无 id)
{"jsonrpc": "2.0", "method": "message", "params": {"chat": "...", "content": "..."}}
```

支持方法：`status`、`send`、`send_image`、`chat`、`listen`、`unlisten`、`contacts`、`sessions`、`screenshot`

> REST API 保持 100% 向后兼容。

---

## 🚀 快速开始

### 环境要求

- Linux x86_64 系统 (Ubuntu 22.04+ 推荐)，或可运行 x86_64 容器的主机
- Docker + Docker Compose
- 允许 `SYS_ADMIN` / `SYS_PTRACE` 能力 (密钥提取 + fanotify 需要)

### 生产部署

```bash
git clone https://github.com/PigeonCoders/MimicWX-Linux.git
cd MimicWX-Linux
docker compose up -d
```

**扫码登录（无需 VNC）：**

服务启动后，API 会在登录前即可用。有两种方式获取登录二维码：

```bash
# 方式一: 查看容器日志，终端会自动渲染二维码
docker logs -f mimicwx-linux

# 方式二: 通过截图 API 获取二维码图片
curl http://HOST:8899/screenshot -o qr.png
# 或直接在浏览器打开 http://HOST:8899/screenshot
```

登录完成后服务自动进入 Serving 状态，后续可随时通过 `/screenshot` 查看微信运行状态。

> 微信登录状态保存在 `wechat-data` volume 中，重启容器无需重新登录。

**可选: VNC debug 模式**

如需完整远程桌面（排查问题时），可使用 debug 模式：

```bash
docker compose --profile debug up -d mimicwx-debug
# 浏览器打开 http://HOST:6080/vnc.html (密码: mimicwx)
```

### 本地开发

使用 `Dockerfile.dev` 构建开发镜像（含 VNC + Rust 工具链 + cargo-watch）：

```bash
make up       # 构建 dev 镜像并启动（首次需要通过 noVNC 扫码）
make dev      # 容器内 cargo-watch 热编译（监听 src/ 变化自动重启）
```

其他 make 命令：

| 命令 | 说明 |
|------|------|
| `make up` | 构建 dev 镜像并启动 |
| `make start` | 启动已构建的 dev 容器 |
| `make stop` | 停止容器 |
| `make dev` | 容器内热编译开发模式 |
| `make build` | 容器内手动编译一次 |
| `make shell` | 进入容器 shell |
| `make logs` | 查看容器日志 |
| `make attach` | 进入交互式控制台 (`Ctrl+P Ctrl+Q` 退出) |

### 环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `MIMICWX_DEBUG` | `0` | 设为 `1` 启用 VNC + noVNC debug 模式 |
| `MIMICWX_API_PORT` | `8899` | API 监听端口 |
| `MIMICWX_VNC_PORT` | `5901` | VNC 端口 (仅 debug 模式) |
| `MIMICWX_NOVNC_PORT` | `6080` | noVNC 端口 (仅 debug 模式) |
| `MIMICWX_DISPLAY_NUM` | `1` | X11 DISPLAY 编号 |

### 访问入口

| 服务 | 地址 | 说明 |
|------|------|------|
| API | `http://HOST:8899` | REST API 接口 |
| 截图 | `http://HOST:8899/screenshot` | 微信窗口截图 (免认证，可用于扫码登录) |
| WebSocket | `ws://HOST:8899/ws` | JSON-RPC 2.0 + 实时事件推送 |
| noVNC | `http://HOST:6080/vnc.html` | 浏览器远程桌面 (仅 debug 模式，密码: `mimicwx`) |
| VNC | `vnc://HOST:5901` | VNC 客户端连接 (仅 debug 模式) |

---

## ⚙️ 配置文件

`config.toml` — 配置搜索优先级: `./config.toml` → `/home/wechat/mimicwx-linux/config.toml` → `/etc/mimicwx/config.toml`

```toml
[api]
# API 认证 Token (留空则不启用认证)
# 请求方式: Header "Authorization: Bearer <token>" 或 Query "?token=<token>"
token = "your-secret-token"

[listen]
# 启动后自动弹出独立窗口并监听的对象
# 填入联系人名称或群名称 (与微信显示名一致)
auto = ["文件传输助手", "好友A", "工作群"]
```

---

## 🔧 对接 Yunzai-Bot

项目内置 Yunzai-Bot v3 适配器 (`adapter/MimicWX.js`)，支持：

- WebSocket 实时消息接收
- 自动解析数据库消息 (文本/图片/语音/视频/表情/链接)
- 智能消息分段发送 (文本 + 图片分离)
- 私聊/群聊消息路由
- 好友/群列表自动同步

```bash
# 环境变量
export MIMICWX_URL="http://localhost:8899"      # API 地址
export MIMICWX_TOKEN="your-secret-token"         # 认证 Token
```

---

## 🔑 密钥提取原理

```
WeChat 进程启动
      │
      ▼
GDB attach (start.sh 自动触发)
      │
      ▼
在 setCipherKey 偏移 (0x6586C90) 设断点
      │
      ▼
用户扫码登录 → 微信调用 setCipherKey 打开数据库
      │
      ▼
断点触发 → 从 $rsi 寄存器读取 Data 结构体
      │
      ▼
提取 32 字节 AES 密钥 → 保存至 /tmp/wechat_key.txt
      │
      ▼
GDB detach → 微信正常运行 → MimicWX 读取密钥 → 解密数据库
```

> ⚠️ 密钥偏移量 `0x6586C90` 对应 WeChat Linux 4.1.0.16 版本，升级微信后可能需要更新。

---

## 🛠️ 技术栈

| 组件 | 技术 | 说明 |
|------|------|------|
| 语言 | **Rust** | 异步高性能，零运行时开销 |
| 异步运行时 | **Tokio** | 全功能异步运行时 |
| 消息检测 | **SQLCipher** + **fanotify** | 数据库解密 + WAL 实时监听 |
| UI 自动化 | **AT-SPI2** (`atspi-rs` + `zbus`) | D-Bus 无障碍接口控制 |
| 输入注入 | **X11 XTEST** (`x11rb`) | 原生 X11 扩展 + Selection 剪贴板 |
| API 服务 | **axum** | HTTP + WebSocket (JSON-RPC 2.0) |
| 序列化 | **serde** + **serde_json** | JSON 序列化/反序列化 |
| XML 解析 | **quick-xml** | 微信消息 XML 解析 |
| 压缩 | **zstd** | WCDB Zstd BLOB 解压 |
| 容器化 | **Docker** (Ubuntu 22.04) | 多阶段构建 |
| 虚拟桌面 | **Xvfb** + **openbox** | 默认 headless，可选 VNC debug |
| 密钥提取 | **GDB** + **Python** | 运行时内存断点 |

---

## 📊 启动流程

```
容器启动 (start.sh)
 ├── 0) 系统服务: D-Bus daemon + ptrace 设置 + 权限修复
 ├── 1) D-Bus session bus
 ├── 2) 显示服务:
 │      ├─ headless 模式: Xvfb + openbox (默认)
 │      └─ debug 模式:   VNC + XFCE (MIMICWX_DEBUG=1)
 ├── 3) 清理冗余 AT-SPI2 (避免 bus 冲突)
 ├── 4) 启动唯一的 AT-SPI2 bus
 ├── 5) 获取 AT-SPI2 bus 地址 → 保存环境变量
 ├── 6) 启动微信 → 等待窗口就绪
 ├── GDB 密钥提取 (后台, 等待用户扫码)
 ├── 7) noVNC websockify (仅 debug 模式)
 └── 8) MimicWX 主服务 (状态机驱动)
      ├── Booting       → AT-SPI2 连接 + X11 XTEST 输入引擎
      ├── DesktopReady  → 等待微信进程
      ├── WeChatReady   → 检测登录状态
      ├── API 服务启动  → /status + /screenshot 立即可用 (登录前)
      ├── LoginWaiting  → 终端自动渲染二维码 + /screenshot 获取二维码
      ├── KeyReady      → 读取 GDB 密钥
      ├── DbReady       → DbManager 初始化 + 联系人加载
      ├── Serving       → 全功能服务 + 消息监听 + 自动监听
      └── Degraded      → 部分功能降级运行
```

---

## 📝 API 使用示例

### 查询状态
```bash
curl http://localhost:8899/status
```

### 获取屏幕截图 (扫码登录)
```bash
# 直接在浏览器打开, 或保存为文件
curl http://localhost:8899/screenshot -o screenshot.png

# 也可直接用 <img> 标签
# <img src="http://HOST:8899/screenshot" />
```

### 发送消息
```bash
curl -X POST http://localhost:8899/send \
  -H "Authorization: Bearer your-token" \
  -H "Content-Type: application/json" \
  -d '{"to": "文件传输助手", "text": "Hello from MimicWX!"}'
```

### 发送图片 (base64)
```bash
curl -X POST http://localhost:8899/send_image \
  -H "Authorization: Bearer your-token" \
  -H "Content-Type: application/json" \
  -d '{"to": "文件传输助手", "file": "<base64-data>", "name": "test.png"}'
```

### 添加监听
```bash
curl -X POST http://localhost:8899/listen \
  -H "Authorization: Bearer your-token" \
  -H "Content-Type: application/json" \
  -d '{"who": "好友A"}'
```

### WebSocket 连接 (JSON-RPC 2.0)
```javascript
const ws = new WebSocket("ws://localhost:8899/ws?token=your-token")

// 接收事件推送
ws.onmessage = (e) => console.log(JSON.parse(e.data))

// 通过 WS 发送命令 (无需额外 HTTP 请求)
ws.send(JSON.stringify({
  jsonrpc: "2.0",
  method: "send",
  params: { to: "文件传输助手", text: "Hello!" },
  id: 1
}))
```

---

## 🖥️ 控制台命令

通过 `docker attach mimicwx-linux` 进入交互式控制台：

```
> /help
```

| 命令 | 功能 |
|------|------|
| `/restart` | 优雅重启程序 |
| `/stop` | 正常关闭程序 |
| `/status` | 显示运行时状态 |
| `/refresh` | 手动刷新联系人缓存 |
| `/reload` | 热重载配置文件 |
| `/atmode` | 切换仅@模式 |
| `/send <收件人> <内容>` | 发送消息 |
| `/listen <名称>` | 添加监听 (自动写入 config.toml) |
| `/unlisten <名称>` | 移除监听 (自动写入 config.toml) |
| `/sessions` | 查看会话列表 |
| `/help` | 显示帮助 |

**快捷键**: `↑↓` 历史命令 · `←→` 移动光标 · `Ctrl+U` 清行 · `Ctrl+L` 清屏

> 退出控制台但不停止容器: `Ctrl+P` 然后 `Ctrl+Q`

---

## 📋 更新日志

### v0.6.0

- 🔄 **运行时状态机** — 显式 RuntimeState 枚举驱动启动流程，告别散落的 sleep/文件约定
- 🧩 **Ports/Adapters 架构** — KeyProvider / MessageSource / MessageSender / SessionLocator trait 解耦基础设施和业务
- 📡 **JSON-RPC over WebSocket** — WS 支持双向通信，适配器只需一个连接即可完成所有操作
- 📊 **输入层可观测化** — InputMetrics 实时跟踪队列深度、命令耗时、剪贴板/焦点失败
- 🎯 **NodeHandle 节点稳定性** — 统一的 AT-SPI 节点指纹匹配 + 缓存失效自动重定位
- 🐳 **Headless 默认化** — 默认 Xvfb + openbox 轻量运行，`MIMICWX_DEBUG=1` 按需启用 VNC
- 📢 **类型化事件** — WxEvent 枚举替代裸 JSON 字符串广播，WS 推送遵循 JSON-RPC 2.0

### v0.5.1

- 📡 **微信互通命令** — 主人可通过微信私聊 `#` 命令远程控制 Bot (复用 Yunzai 主人系统)
- 🔄 **配置热重载** — `/reload` 命令重读 config.toml，自动 diff 监听列表并增删
- 💾 **监听持久化** — `/listen` `/unlisten` 自动写入 config.toml，重启不丢失
- 🎮 **控制台命令扩展** — 新增 `/send`、`/listen`、`/unlisten`、`/sessions`、`/reload`、`/atmode`
- 🔧 **独立窗口自动恢复** — 发送消息时检测窗口失效自动重建
- ⚡ **AT-SPI2 轮询替代固定延迟** — 会话切换、搜索、独立窗口弹出改用状态轮询
- ⚙️ **@ 延迟可配置** — `config.toml` 新增 `[timing].at_delay_ms`，支持热更新

### v0.5.0

- ♻️ **移除 AT-SPI 消息读取** — 消息检测全面转向数据库通道，更稳定更高效
- 🔧 **send_message/send_image 公共方法提取** — `check_listen_window` + `prepare_main_send` 减少代码重复
- 🎯 **会话精确匹配优先** — `find_session` 改为精确 > starts_with > contains 优先级策略
- 🔄 **ChatWnd 缓存自动刷新** — 输入框/消息列表节点使用前 bbox 校验，失效自动重新搜索
- ⚡ **X11 Selection 剪贴板** — 文本粘贴改用 X11 Selection 协议，消除 xclip 进程开销
- 🧹 **适配器清理** — 删除 AT-SPI 消息处理器死代码，简化 DB 验证日志

---

## License

MIT
