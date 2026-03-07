//! HTTP API 服务
//!
//! 提供 REST + WebSocket 接口:
//! - GET  /status        — 服务状态 (免认证)
//! - GET  /contacts      — 联系人列表 (数据库)
//! - GET  /sessions      — 会话列表 (优先数据库)
//! - GET  /messages/new  — 增量新消息 (数据库)
//! - POST /send          — 发送消息 (AT-SPI)
//! - POST /chat          — 切换聊天 (AT-SPI)
//! - POST /listen        — 添加监听 (弹出独立窗口)
//! - DELETE /listen      — 移除监听
//! - GET  /listen        — 监听列表
//! - GET  /screenshot    — X11 屏幕截图 (免认证, 用于扫码)
//! - GET  /debug/tree    — AT-SPI2 控件树
//! - GET  /ws            — WebSocket 实时推送

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};
use utoipa::{OpenApi, ToSchema};

use crate::atspi::AtSpi;
use crate::db::DbManager;
use crate::events::WxEvent;
use crate::input::InputEngine;
use crate::runtime::{RuntimeManager, RuntimeSnapshot};
use crate::wechat::WeChat;

// =====================================================================
// 共享状态
// =====================================================================

pub struct AppState {
    pub wechat: Arc<WeChat>,
    pub atspi: Arc<AtSpi>,
    pub runtime: Arc<RuntimeManager>,
    pub input_metrics: Arc<InputMetrics>,
    /// InputEngine 命令队列 (替代 Mutex, 消除长持锁)
    pub input_tx: tokio::sync::mpsc::Sender<InputCommand>,
    pub tx: broadcast::Sender<WxEvent>,
    /// 数据库管理器 (密钥获取成功后通过 OnceLock 设置)
    pub db: Arc<std::sync::OnceLock<Arc<DbManager>>>,
    /// API 认证 Token (None = 不启用认证)
    pub api_token: Option<String>,
    /// 启动时间 (用于 uptime 计算)
    pub start_time: std::time::Instant,
    /// 配置文件路径 (用于 /reload 和 /listen 持久化)
    pub config_path: Option<std::path::PathBuf>,
}

#[derive(Default)]
pub struct InputMetrics {
    queue_depth: AtomicU32,
    total_commands: AtomicU64,
    total_failures: AtomicU64,
    last_command_ms: AtomicU64,
    max_command_ms: AtomicU64,
    clipboard_acquire_failures: AtomicU64,
    focus_lost_count: AtomicU64,
}

#[derive(Serialize, ToSchema)]
pub struct InputMetricsSnapshot {
    /// 当前命令队列深度
    pub queue_depth: u32,
    /// 已执行命令总数
    pub total_commands: u64,
    /// 失败命令总数
    pub total_failures: u64,
    /// 最近一次命令耗时 (毫秒)
    pub last_command_ms: u64,
    /// 历史最大命令耗时 (毫秒)
    pub max_command_ms: u64,
    /// 剪贴板获取失败次数
    pub clipboard_acquire_failures: u64,
    /// 焦点丢失次数
    pub focus_lost_count: u64,
}

impl InputMetrics {
    pub fn on_enqueue(&self) {
        self.queue_depth.fetch_add(1, Ordering::Relaxed);
    }

    pub fn on_enqueue_failed(&self) {
        self.decrement_queue_depth();
    }

    pub fn on_dequeue(&self) {
        self.decrement_queue_depth();
        self.total_commands.fetch_add(1, Ordering::Relaxed);
    }

    pub fn on_finish(&self, elapsed: Duration, failed: bool) {
        let elapsed_ms = elapsed.as_millis() as u64;
        self.last_command_ms.store(elapsed_ms, Ordering::Relaxed);
        self.max_command_ms.fetch_max(elapsed_ms, Ordering::Relaxed);
        if failed {
            self.total_failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_clipboard_failure(&self) {
        self.clipboard_acquire_failures
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_focus_lost(&self) {
        self.focus_lost_count.fetch_add(1, Ordering::Relaxed);
    }

    fn decrement_queue_depth(&self) {
        let mut current = self.queue_depth.load(Ordering::Relaxed);
        loop {
            if current == 0 {
                return;
            }
            match self.queue_depth.compare_exchange(
                current,
                current - 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    pub fn snapshot(&self) -> InputMetricsSnapshot {
        InputMetricsSnapshot {
            queue_depth: self.queue_depth.load(Ordering::Relaxed),
            total_commands: self.total_commands.load(Ordering::Relaxed),
            total_failures: self.total_failures.load(Ordering::Relaxed),
            last_command_ms: self.last_command_ms.load(Ordering::Relaxed),
            max_command_ms: self.max_command_ms.load(Ordering::Relaxed),
            clipboard_acquire_failures: self.clipboard_acquire_failures.load(Ordering::Relaxed),
            focus_lost_count: self.focus_lost_count.load(Ordering::Relaxed),
        }
    }
}

// =====================================================================
// InputEngine Actor
// =====================================================================

use tokio::sync::oneshot;

/// InputEngine 命令 (经 mpsc 队列发送给 actor)
pub enum InputCommand {
    SendMessage {
        to: String,
        text: String,
        at: Vec<String>,
        skip_verify: bool,
        reply: oneshot::Sender<anyhow::Result<(bool, bool, String)>>,
    },
    SendImage {
        to: String,
        image_path: String,
        reply: oneshot::Sender<anyhow::Result<(bool, bool, String)>>,
    },
    ChatWith {
        who: String,
        reply: oneshot::Sender<anyhow::Result<Option<String>>>,
    },
    AddListen {
        who: String,
        reply: oneshot::Sender<anyhow::Result<bool>>,
    },
    RemoveListen {
        who: String,
        reply: oneshot::Sender<bool>,
    },
}

/// 启动 InputEngine actor (在独立 task 中顺序执行命令)
pub fn spawn_input_actor(
    mut engine: InputEngine,
    wechat: Arc<WeChat>,
    metrics: Arc<InputMetrics>,
    mut rx: tokio::sync::mpsc::Receiver<InputCommand>,
) {
    tokio::spawn(async move {
        debug!("InputEngine actor 已启动");
        while let Some(cmd) = rx.recv().await {
            metrics.on_dequeue();
            match cmd {
                InputCommand::SendMessage {
                    to,
                    text,
                    at,
                    skip_verify,
                    reply,
                } => {
                    let start = Instant::now();
                    let result = tokio::time::timeout(Duration::from_secs(30), async {
                        if !wechat.check_listen_window(&to).await {
                            wechat.try_recover_listen_window(&mut engine, &to).await;
                        }
                        wechat
                            .send_message(&mut engine, &to, &text, &at, skip_verify)
                            .await
                    })
                    .await
                    .map_err(|_| anyhow::anyhow!("Input command timeout after 30s"))
                    .and_then(|result| result);
                    note_input_result(&metrics, start.elapsed(), result.as_ref().err());
                    let _ = reply.send(result);
                }
                InputCommand::SendImage {
                    to,
                    image_path,
                    reply,
                } => {
                    let start = Instant::now();
                    let result = tokio::time::timeout(Duration::from_secs(30), async {
                        if !wechat.check_listen_window(&to).await {
                            wechat.try_recover_listen_window(&mut engine, &to).await;
                        }
                        wechat.send_image(&mut engine, &to, &image_path).await
                    })
                    .await
                    .map_err(|_| anyhow::anyhow!("Input command timeout after 30s"))
                    .and_then(|result| result);
                    note_input_result(&metrics, start.elapsed(), result.as_ref().err());
                    let _ = reply.send(result);
                }
                InputCommand::ChatWith { who, reply } => {
                    let start = Instant::now();
                    let result = tokio::time::timeout(
                        Duration::from_secs(30),
                        wechat.chat_with(&mut engine, &who),
                    )
                    .await
                    .map_err(|_| anyhow::anyhow!("Input command timeout after 30s"))
                    .and_then(|result| result);
                    note_input_result(&metrics, start.elapsed(), result.as_ref().err());
                    let _ = reply.send(result);
                }
                InputCommand::AddListen { who, reply } => {
                    let start = Instant::now();
                    let result = tokio::time::timeout(
                        Duration::from_secs(30),
                        wechat.add_listen(&mut engine, &who),
                    )
                    .await
                    .map_err(|_| anyhow::anyhow!("Input command timeout after 30s"))
                    .and_then(|result| result);
                    note_input_result(&metrics, start.elapsed(), result.as_ref().err());
                    let _ = reply.send(result);
                }
                InputCommand::RemoveListen { who, reply } => {
                    let start = Instant::now();
                    let result = tokio::time::timeout(
                        Duration::from_secs(30),
                        wechat.remove_listen(&engine, &who),
                    )
                    .await;
                    metrics.on_finish(start.elapsed(), result.is_err());
                    let _ = reply.send(result.unwrap_or(false));
                }
            }
        }
        debug!("InputEngine actor 已停止");
    });
}

fn note_input_result(metrics: &InputMetrics, elapsed: Duration, error: Option<&anyhow::Error>) {
    if let Some(error) = error {
        let text = error.to_string();
        if text.contains("CLIPBOARD ownership") || text.contains("剪贴板") {
            metrics.record_clipboard_failure();
        }
        if text.contains("focus lost") {
            metrics.record_focus_lost();
        }
    }
    metrics.on_finish(elapsed, error.is_some());
}

// =====================================================================
// 工具函数
// =====================================================================

/// 简单的 URL percent decode (%XX → 字节)
fn percent_decode(input: &str) -> String {
    let mut bytes = Vec::with_capacity(input.len());
    let mut chars = input.as_bytes().iter();
    while let Some(&b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().copied().unwrap_or(0);
            let lo = chars.next().copied().unwrap_or(0);
            if let (Some(h), Some(l)) = (hex_val(hi), hex_val(lo)) {
                bytes.push(h << 4 | l);
                continue;
            }
        }
        bytes.push(b);
    }
    String::from_utf8(bytes).unwrap_or_else(|_| input.to_string())
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 轻量伪随机 u16 (无需引入 rand crate, 用时间纳秒低位)
fn rand_u16() -> u16 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (t.subsec_nanos() ^ (t.as_millis() as u32)) as u16
}

pub async fn enqueue_input_command(
    tx: &tokio::sync::mpsc::Sender<InputCommand>,
    metrics: &InputMetrics,
    cmd: InputCommand,
) -> Result<(), ()> {
    metrics.on_enqueue();
    if tx.send(cmd).await.is_err() {
        metrics.on_enqueue_failed();
        return Err(());
    }
    Ok(())
}

// =====================================================================
// 统一错误响应
// =====================================================================

/// API 错误类型 (带 HTTP 状态码)
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn unavailable(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: msg.into(),
        }
    }
    fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let body = serde_json::json!({ "error": self.message });
        (self.status, Json(body)).into_response()
    }
}

// =====================================================================
// 认证中间件
// =====================================================================

/// Token 认证中间件
/// 检查 Header `Authorization: Bearer <token>` 或 Query `?token=<token>`
async fn auth_layer(
    State(state): State<Arc<AppState>>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Result<impl IntoResponse, StatusCode> {
    let token = match &state.api_token {
        Some(t) => t,
        None => return Ok(next.run(req).await), // 未配置 token, 跳过认证
    };

    // 1. 检查 Authorization header
    if let Some(auth) = req.headers().get("authorization") {
        if let Ok(auth_str) = auth.to_str() {
            if let Some(bearer) = auth_str.strip_prefix("Bearer ") {
                if bearer.trim() == token {
                    return Ok(next.run(req).await);
                }
            }
        }
    }

    // 2. 检查 query param ?token=xxx (需 URL decode)
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(val) = pair.strip_prefix("token=") {
                // URL decode: %23 → #, %20 → space, etc.
                let decoded = percent_decode(val);
                if decoded == *token {
                    return Ok(next.run(req).await);
                }
            }
        }
    }

    warn!("🔒 API 认证失败: {}", req.uri().path());
    Err(StatusCode::UNAUTHORIZED)
}

// =====================================================================
// 路由
// =====================================================================

// =====================================================================
// OpenAPI documentation
// =====================================================================

use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityScheme};

struct SecurityAddon;

impl utoipa::Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer",
                SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
            );
        }
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "MimicWX-Linux API",
        description = "微信自动化框架 REST API。通过 AT-SPI2 无障碍接口 + X11 XTEST 输入注入实现零风险微信自动化。\n\n## 认证方式\n- Header: `Authorization: Bearer <token>`\n- Query: `?token=<token>` (支持 URL 编码)\n- 未配置 Token 时自动跳过认证\n\n## 免认证接口\n- `GET /status` — 服务状态\n- `GET /screenshot` — 屏幕截图 (用于扫码登录)\n\n## 实时推送\n- `GET /ws` — WebSocket 连接, 支持 JSON-RPC 2.0 双向通信\n\n## 超时机制\n所有操作类接口 (发送、切换、监听) 均有 30 秒超时限制。",
        version = env!("CARGO_PKG_VERSION"),
    ),
    paths(
        get_status, get_screenshot,
        get_contacts, get_new_messages, get_sessions,
        send_message, send_image, chat_with,
        get_listen_list, add_listen, remove_listen,
        get_tree, get_session_tree, exec_command,
    ),
    components(schemas(
        StatusResponse, RuntimeSnapshot, InputMetricsSnapshot,
        SendRequest, SendImageRequest, SendResponse,
        ChatRequest, ChatResponse,
        ListenRequest, ListenResponse,
        CommandReqSchema,
    )),
    modifiers(&SecurityAddon),
    tags(
        (name = "Status", description = "服务状态与截图"),
        (name = "Data", description = "联系人、会话与消息"),
        (name = "Actions", description = "发送消息、切换聊天、执行命令"),
        (name = "Listen", description = "监听管理"),
        (name = "Debug", description = "AT-SPI2 调试工具"),
    )
)]
struct ApiDoc;

pub fn build_router(state: Arc<AppState>) -> Router {
    // 需要认证的路由
    let protected = Router::new()
        .route("/contacts", get(get_contacts))
        .route("/messages/new", get(get_new_messages))
        .route("/send", post(send_message))
        .route("/send_image", post(send_image))
        .route("/sessions", get(get_sessions))
        .route("/chat", post(chat_with))
        .route("/listen", get(get_listen_list))
        .route("/listen", post(add_listen))
        .route("/listen", delete(remove_listen))
        .route("/command", post(exec_command))
        .route("/debug/tree", get(get_tree))
        .route("/debug/sessions", get(get_session_tree))
        .route("/ws", get(ws_handler))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth_layer));

    // 免认证路由
    Router::new()
        .route("/status", get(get_status))
        .route("/screenshot", get(get_screenshot))
        .merge(protected)
        .merge(
            utoipa_swagger_ui::SwaggerUi::new("/docs")
                .url("/api-docs/openapi.json", ApiDoc::openapi()),
        )
        .layer(tower_http::cors::CorsLayer::permissive()) // ⑩ CORS 支持
        .with_state(state)
}

// =====================================================================
// 请求/响应类型
// =====================================================================

#[derive(Serialize, ToSchema)]
struct StatusResponse {
    /// 微信连接状态 (如 "Connected", "Disconnected")
    status: String,
    /// 运行时状态 (state + 可选 reason)
    runtime: RuntimeSnapshot,
    /// 输入引擎性能指标
    input_metrics: InputMetricsSnapshot,
    /// MimicWX 版本号 (如 "0.5.1")
    version: String,
    /// 当前活跃监听数量
    listen_count: usize,
    /// 数据库是否可用 (密钥已解密)
    db_available: bool,
    /// 数据库中的联系人总数
    contacts: usize,
    /// 服务运行时长 (秒)
    uptime_secs: u64,
}

#[derive(Deserialize, ToSchema)]
struct SendRequest {
    /// 接收者名称 (联系人或群组)
    to: String,
    /// 消息文本
    text: String,
    /// 需要 @ 的显示名称列表 (可选)
    #[serde(default)]
    at: Vec<String>,
}

#[derive(Deserialize, ToSchema)]
struct SendImageRequest {
    /// 接收者名称 (联系人或群组)
    to: String,
    /// Base64 编码的图片数据
    file: String,
    /// 文件名 (可选, 用于推断 MIME 类型)
    #[serde(default = "default_image_name")]
    name: String,
}

fn default_image_name() -> String {
    "image.png".to_string()
}

#[derive(Serialize, ToSchema)]
struct SendResponse {
    /// 消息是否已发送
    sent: bool,
    /// 消息是否已验证送达
    verified: bool,
    /// 状态信息
    message: String,
}

#[derive(Deserialize, ToSchema)]
struct ChatRequest {
    /// 要切换到的联系人或群组名称
    who: String,
}

#[derive(Serialize, ToSchema)]
struct ChatResponse {
    /// 切换是否成功
    success: bool,
    /// 切换后的当前聊天名称
    chat_name: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct ListenRequest {
    /// 联系人或群组名称
    who: String,
}

#[derive(Serialize, ToSchema)]
struct ListenResponse {
    /// 操作是否成功
    success: bool,
    /// 状态信息
    message: String,
}

#[derive(Deserialize, ToSchema)]
#[allow(dead_code)]
struct CommandReqSchema {
    /// 命令字符串 (如 "status", "send <to> <text>", "listen <who>")
    cmd: String,
}

// =====================================================================
// Handlers
// =====================================================================

#[utoipa::path(
    get, path = "/status",
    tag = "Status",
    summary = "服务状态 (免认证)",
    description = "获取 MimicWX 服务的完整状态快照, 包括:\n- 微信连接状态\n- 运行时状态 (Running / LoginWaiting 等)\n- 输入引擎指标 (队列深度、命令耗时、失败次数)\n- 监听数量、数据库可用性、联系人数量、运行时长\n\n此接口免认证, 可用于健康检查和监控。",
    responses((status = 200, description = "当前服务状态", body = StatusResponse))
)]
async fn get_status(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    let status = state.wechat.check_status().await;
    let runtime = state.runtime.snapshot().await;
    let input_metrics = state.input_metrics.snapshot();
    let listen_count = state.wechat.get_listen_list().await.len();
    let db_available = state.db.get().is_some();
    let contacts = if let Some(d) = state.db.get() {
        d.get_contacts().await.len()
    } else {
        0
    };
    let uptime_secs = state.start_time.elapsed().as_secs();
    Json(StatusResponse {
        status: status.to_string(),
        runtime,
        input_metrics,
        version: env!("CARGO_PKG_VERSION").into(),
        listen_count,
        db_available,
        contacts,
        uptime_secs,
    })
}

#[utoipa::path(
    get, path = "/screenshot",
    tag = "Status",
    summary = "X11 屏幕截图 (免认证, 用于扫码)",
    description = "通过 X11 截取当前屏幕画面并返回 PNG 图片。\n\n主要用于远程扫码登录微信: 获取截图后识别二维码完成登录。\n此接口免认证, 在阻塞线程中执行截屏操作。",
    responses(
        (status = 200, description = "PNG 图片", content_type = "image/png"),
        (status = 500, description = "截图失败")
    )
)]
async fn get_screenshot() -> Result<impl IntoResponse, ApiError> {
    let png_data = tokio::task::spawn_blocking(crate::input::capture_screenshot)
        .await
        .map_err(|e| ApiError::internal(format!("截屏任务失败: {e}")))?
        .map_err(|e| ApiError::internal(format!("截屏失败: {e}")))?;

    Ok(([(axum::http::header::CONTENT_TYPE, "image/png")], png_data))
}

async fn send_message_inner(
    state: &Arc<AppState>,
    req: SendRequest,
) -> Result<SendResponse, ApiError> {
    let has_db = state.db.get().is_some();
    let sent_rx = state.db.get().map(|db| db.subscribe_sent());

    let (reply_tx, reply_rx) = oneshot::channel();
    enqueue_input_command(
        &state.input_tx,
        &state.input_metrics,
        InputCommand::SendMessage {
            to: req.to.clone(),
            text: req.text.clone(),
            at: req.at.clone(),
            skip_verify: true,
            reply: reply_tx,
        },
    )
    .await
    .map_err(|_| ApiError::unavailable("InputEngine actor 已停止"))?;

    match reply_rx.await {
        Ok(Ok((sent, atspi_verified, message))) => {
            let verified = if let Some(rx) = sent_rx {
                state
                    .db
                    .get()
                    .unwrap()
                    .verify_sent(&req.text, rx)
                    .await
                    .unwrap_or(atspi_verified)
            } else if has_db {
                atspi_verified
            } else {
                state
                    .wechat
                    .verify_sent_after_send(&req.to, &req.text)
                    .await
            };

            let _ = state.tx.send(WxEvent::Sent {
                to: req.to,
                text: req.text,
                verified,
            });
            Ok(SendResponse {
                sent,
                verified,
                message,
            })
        }
        Ok(Err(e)) => Err(ApiError::internal(format!("发送失败: {e}"))),
        Err(_) => Err(ApiError::internal("actor 响应通道已关闭")),
    }
}

async fn send_image_inner(
    state: &Arc<AppState>,
    req: SendImageRequest,
) -> Result<SendResponse, ApiError> {
    use std::io::Write;

    use base64::Engine;
    let image_data = base64::engine::general_purpose::STANDARD
        .decode(&req.file)
        .map_err(|e| ApiError::internal(format!("base64 解码失败: {e}")))?;

    let ext = if req.name.contains('.') {
        req.name.rsplit('.').next().unwrap_or("png")
    } else {
        "png"
    };
    let tmp_path = format!(
        "/tmp/mimicwx_img_{}_{:04x}.{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        rand_u16(),
        ext
    );
    {
        let mut f = std::fs::File::create(&tmp_path)
            .map_err(|e| ApiError::internal(format!("创建临时文件失败: {e}")))?;
        f.write_all(&image_data)
            .map_err(|e| ApiError::internal(format!("写入图片失败: {e}")))?;
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    enqueue_input_command(
        &state.input_tx,
        &state.input_metrics,
        InputCommand::SendImage {
            to: req.to,
            image_path: tmp_path.clone(),
            reply: reply_tx,
        },
    )
    .await
    .map_err(|_| ApiError::unavailable("InputEngine actor 已停止"))?;

    let result = reply_rx.await;
    let _ = std::fs::remove_file(&tmp_path);

    match result {
        Ok(Ok((sent, verified, message))) => Ok(SendResponse {
            sent,
            verified,
            message,
        }),
        Ok(Err(e)) => Err(ApiError::internal(format!("发送图片失败: {e}"))),
        Err(_) => Err(ApiError::internal("actor 响应通道已关闭")),
    }
}

async fn chat_with_inner(
    state: &Arc<AppState>,
    req: ChatRequest,
) -> Result<ChatResponse, ApiError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    enqueue_input_command(
        &state.input_tx,
        &state.input_metrics,
        InputCommand::ChatWith {
            who: req.who.clone(),
            reply: reply_tx,
        },
    )
    .await
    .map_err(|_| ApiError::unavailable("InputEngine actor 已停止"))?;

    match reply_rx.await {
        Ok(Ok(Some(name))) => Ok(ChatResponse {
            success: true,
            chat_name: Some(name),
        }),
        Ok(Ok(None)) => Ok(ChatResponse {
            success: false,
            chat_name: None,
        }),
        Ok(Err(e)) => Err(ApiError::internal(format!("切换聊天失败: {e}"))),
        Err(_) => Err(ApiError::internal("actor 响应通道已关闭")),
    }
}

async fn add_listen_inner(
    state: &Arc<AppState>,
    req: ListenRequest,
) -> Result<ListenResponse, ApiError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    enqueue_input_command(
        &state.input_tx,
        &state.input_metrics,
        InputCommand::AddListen {
            who: req.who.clone(),
            reply: reply_tx,
        },
    )
    .await
    .map_err(|_| ApiError::unavailable("InputEngine actor 已停止"))?;

    match reply_rx.await {
        Ok(Ok(true)) => Ok(ListenResponse {
            success: true,
            message: format!("已添加监听: {}", req.who),
        }),
        Ok(Ok(false)) => Ok(ListenResponse {
            success: false,
            message: format!("添加监听失败: {}", req.who),
        }),
        Ok(Err(e)) => Err(ApiError::internal(format!("添加监听错误: {e}"))),
        Err(_) => Err(ApiError::internal("actor 响应通道已关闭")),
    }
}

async fn remove_listen_inner(state: &Arc<AppState>, req: ListenRequest) -> ListenResponse {
    let (reply_tx, reply_rx) = oneshot::channel();
    let sent = enqueue_input_command(
        &state.input_tx,
        &state.input_metrics,
        InputCommand::RemoveListen {
            who: req.who.clone(),
            reply: reply_tx,
        },
    )
    .await;

    let removed = if sent.is_ok() {
        reply_rx.await.unwrap_or(false)
    } else {
        false
    };
    ListenResponse {
        success: removed,
        message: if removed {
            format!("已移除监听: {}", req.who)
        } else {
            format!("未找到监听: {}", req.who)
        },
    }
}

async fn contacts_value(state: &Arc<AppState>) -> Result<Value, ApiError> {
    let db = state
        .db
        .get()
        .ok_or_else(|| ApiError::unavailable("数据库不可用"))?;
    Ok(serde_json::json!({ "contacts": db.get_contacts().await }))
}

async fn sessions_value(state: &Arc<AppState>) -> Value {
    if let Some(db) = state.db.get() {
        match db.get_sessions().await {
            Ok(sessions) => return serde_json::to_value(sessions).unwrap_or_default(),
            Err(e) => tracing::warn!("数据库会话查询失败, fallback AT-SPI: {}", e),
        }
    }
    serde_json::to_value(state.wechat.list_sessions().await).unwrap_or_default()
}

#[utoipa::path(
    get, path = "/contacts",
    tag = "Data",
    summary = "联系人列表 (来自数据库)",
    description = "从微信 SQLCipher 数据库读取完整联系人列表。\n\n需要数据库已解密并可用 (密钥自动从内存提取)。\n返回格式: `{ \"contacts\": [...] }`",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "联系人列表 `{ \"contacts\": [...] }`"),
        (status = 401, description = "未授权 — Token 缺失或不匹配"),
        (status = 503, description = "数据库不可用 — 密钥尚未获取或数据库未初始化")
    )
)]
async fn get_contacts(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(contacts_value(&state).await?))
}

#[utoipa::path(
    get, path = "/messages/new",
    tag = "Data",
    summary = "增量新消息 (来自数据库)",
    description = "从微信数据库增量拉取新消息。每次调用返回上次拉取后的新消息, 适合轮询场景。\n\n需要数据库已解密并可用。",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "新消息数组"),
        (status = 401, description = "未授权 — Token 缺失或不匹配"),
        (status = 503, description = "数据库不可用 — 密钥尚未获取或数据库未初始化")
    )
)]
async fn get_new_messages(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    let db = state
        .db
        .get()
        .ok_or_else(|| ApiError::unavailable("数据库不可用"))?;
    match db.get_new_messages().await {
        Ok(msgs) => Ok(Json(serde_json::to_value(msgs).unwrap_or_default())),
        Err(e) => Err(ApiError::internal(format!("消息查询失败: {e}"))),
    }
}

#[utoipa::path(
    post, path = "/send",
    tag = "Actions",
    summary = "发送文本消息",
    description = "向指定联系人或群组发送文本消息。\n\n**执行流程:**\n1. 检查目标的监听窗口是否存在, 若丢失则自动恢复\n2. 通过 AT-SPI 定位聊天窗口并注入文本\n3. 支持 `at` 字段批量 @ 群成员\n4. 双重验证送达: 先通过 AT-SPI 控件树验证, 再通过数据库确认\n\n**超时:** 30 秒\n\n**返回字段:**\n- `sent`: 消息是否成功发出\n- `verified`: 是否确认送达 (数据库验证 > AT-SPI 验证 > 控件树回查)\n- `message`: 状态描述",
    security(("bearer" = [])),
    request_body = SendRequest,
    responses(
        (status = 200, description = "发送结果", body = SendResponse),
        (status = 401, description = "未授权 — Token 缺失或不匹配"),
        (status = 500, description = "发送失败 — AT-SPI 操作异常或超时"),
        (status = 503, description = "输入引擎不可用 — Actor 队列已关闭")
    )
)]
async fn send_message(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SendRequest>,
) -> Result<Json<SendResponse>, ApiError> {
    Ok(Json(send_message_inner(&state, req).await?))
}

#[utoipa::path(
    post, path = "/send_image",
    tag = "Actions",
    summary = "发送图片 (Base64 编码)",
    description = "向指定联系人或群组发送图片。\n\n**请求参数:**\n- `to`: 接收者名称\n- `file`: 图片的 Base64 编码数据\n- `name`: 文件名 (可选, 默认 `image.png`, 用于推断格式)\n\n**执行流程:**\n1. 解码 Base64 数据并写入临时文件 (`/tmp/mimicwx_img_*`)\n2. 检查并恢复目标监听窗口\n3. 通过微信文件发送功能注入图片\n4. 发送完成后自动清理临时文件\n\n**超时:** 30 秒",
    security(("bearer" = [])),
    request_body = SendImageRequest,
    responses(
        (status = 200, description = "发送结果", body = SendResponse),
        (status = 401, description = "未授权 — Token 缺失或不匹配"),
        (status = 500, description = "发送失败 — Base64 解码错误、文件写入失败或 AT-SPI 异常"),
        (status = 503, description = "输入引擎不可用 — Actor 队列已关闭")
    )
)]
async fn send_image(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SendImageRequest>,
) -> Result<Json<SendResponse>, ApiError> {
    Ok(Json(send_image_inner(&state, req).await?))
}

#[utoipa::path(
    get, path = "/sessions",
    tag = "Data",
    summary = "会话列表 (优先数据库, 回退 AT-SPI)",
    description = "获取微信会话列表 (最近聊天)。\n\n**数据源优先级:**\n1. 优先从 SQLCipher 数据库查询 (数据更完整)\n2. 数据库不可用或查询失败时, 回退到 AT-SPI2 控件树解析\n\nAT-SPI 方式仅能获取当前可见的会话列表。",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "会话列表数组"),
        (status = 401, description = "未授权 — Token 缺失或不匹配")
    )
)]
async fn get_sessions(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(sessions_value(&state).await)
}

#[utoipa::path(
    post, path = "/chat",
    tag = "Actions",
    summary = "切换当前聊天",
    description = "切换微信主窗口的当前聊天对象。\n\n通过 AT-SPI2 在会话列表中搜索并点击目标联系人/群组, 使其成为活跃聊天。\n\n**返回字段:**\n- `success`: 是否成功切换\n- `chat_name`: 切换后的聊天名称 (失败时为 null)\n\n**超时:** 30 秒",
    security(("bearer" = [])),
    request_body = ChatRequest,
    responses(
        (status = 200, description = "切换结果", body = ChatResponse),
        (status = 401, description = "未授权 — Token 缺失或不匹配"),
        (status = 500, description = "切换失败 — 未找到目标会话或 AT-SPI 异常"),
        (status = 503, description = "输入引擎不可用 — Actor 队列已关闭")
    )
)]
async fn chat_with(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, ApiError> {
    Ok(Json(chat_with_inner(&state, req).await?))
}

#[utoipa::path(
    post, path = "/listen",
    tag = "Listen",
    summary = "添加监听 (打开独立聊天窗口)",
    description = "为指定联系人或群组添加消息监听。\n\n**执行流程:**\n1. 在微信中搜索目标联系人/群组\n2. 弹出独立的聊天窗口 (与主窗口分离)\n3. 通过 AT-SPI2 持续监听该窗口的新消息\n\n独立窗口确保监听不会干扰主窗口的正常操作。若监听窗口意外关闭, 发送消息时会自动恢复。\n\n**超时:** 30 秒",
    security(("bearer" = [])),
    request_body = ListenRequest,
    responses(
        (status = 200, description = "监听结果", body = ListenResponse),
        (status = 401, description = "未授权 — Token 缺失或不匹配"),
        (status = 500, description = "监听失败 — 未找到目标或无法弹出独立窗口"),
        (status = 503, description = "输入引擎不可用 — Actor 队列已关闭")
    )
)]
async fn add_listen(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ListenRequest>,
) -> Result<Json<ListenResponse>, ApiError> {
    Ok(Json(add_listen_inner(&state, req).await?))
}

#[utoipa::path(
    delete, path = "/listen",
    tag = "Listen",
    summary = "移除监听",
    description = "移除指定联系人或群组的消息监听, 并关闭对应的独立聊天窗口。\n\n若目标不在监听列表中, `success` 返回 false。",
    security(("bearer" = [])),
    request_body = ListenRequest,
    responses(
        (status = 200, description = "移除结果", body = ListenResponse),
        (status = 401, description = "未授权 — Token 缺失或不匹配")
    )
)]
async fn remove_listen(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ListenRequest>,
) -> Json<ListenResponse> {
    Json(remove_listen_inner(&state, req).await)
}

#[utoipa::path(
    get, path = "/listen",
    tag = "Listen",
    summary = "获取监听列表",
    description = "获取当前所有活跃监听的联系人/群组名称列表。\n\n返回字符串数组, 如 `[\"张三\", \"工作群\"]`。",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "监听名称数组, 如 `[\"张三\", \"工作群\"]`"),
        (status = 401, description = "未授权 — Token 缺失或不匹配")
    )
)]
async fn get_listen_list(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let list = state.wechat.get_listen_list().await;
    Json(list)
}

#[utoipa::path(
    get, path = "/debug/tree",
    tag = "Debug",
    summary = "AT-SPI2 控件树",
    description = "导出微信应用的 AT-SPI2 无障碍控件树, 用于调试和开发。\n\n返回嵌套的控件节点数组, 包含角色、名称、状态等属性。可通过 `depth` 参数控制遍历深度。",
    security(("bearer" = [])),
    params(("depth" = Option<u32>, Query, description = "最大树深度 (默认 5, 最大 15)")),
    responses(
        (status = 200, description = "控件树节点数组 (微信未运行时返回空数组)"),
        (status = 401, description = "未授权 — Token 缺失或不匹配")
    )
)]
async fn get_tree(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let max_depth = params
        .get("depth")
        .and_then(|d| d.parse::<u32>().ok())
        .unwrap_or(5)
        .min(15);
    if let Some(app) = state.wechat.find_app().await {
        let tree = state.atspi.dump_tree(&app, max_depth).await;
        Json(tree)
    } else {
        Json(vec![])
    }
}

#[utoipa::path(
    get, path = "/debug/sessions",
    tag = "Debug",
    summary = "会话容器子树 (调试)",
    description = "导出微信会话列表容器的 AT-SPI2 子树 (固定深度 4)。\n\n用于调试会话列表的控件结构, 查看各会话项的名称、角色和层级关系。",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "会话容器控件树 (微信未运行或未找到容器时返回空数组)"),
        (status = 401, description = "未授权 — Token 缺失或不匹配")
    )
)]
async fn get_session_tree(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Some(app) = state.wechat.find_app().await {
        if let Some(container) = state.wechat.find_session_list(&app).await {
            let tree = state.atspi.dump_tree(&container, 4).await;
            return Json(tree);
        }
    }
    Json(vec![])
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.tx.subscribe();
    debug!("🔌 WebSocket 连接建立");

    let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(30));
    ping_interval.tick().await; // 跳过首次

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(event) => {
                        let payload = event.to_jsonrpc_notification().to_string();
                        if socket.send(Message::Text(payload.into())).await.is_err() { break; }
                    }
                    Err(_) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Pong(_))) => {} // 心跳响应
                    Some(Ok(Message::Text(text))) => {
                        if let Some(response) = handle_ws_rpc(&state, text.as_str()).await {
                            if socket.send(Message::Text(response.into())).await.is_err() { break; }
                        }
                    }
                    _ => {}
                }
            }
            _ = ping_interval.tick() => {
                // ⑴ WebSocket 心跳: 每 30s 发 Ping
                if socket.send(Message::Ping(vec![].into())).await.is_err() { break; }
            }
        }
    }

    debug!("🔌 WebSocket 连接断开");
}

#[derive(Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    method: String,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    id: Option<Value>,
}

async fn handle_ws_rpc(state: &Arc<AppState>, raw: &str) -> Option<String> {
    let parsed = match serde_json::from_str::<JsonRpcRequest>(raw) {
        Ok(req) => req,
        Err(_) => return None,
    };

    if parsed.jsonrpc != "2.0" {
        return Some(jsonrpc_error(parsed.id, -32600, "invalid jsonrpc version"));
    }

    let response = match dispatch_ws_method(state, &parsed.method, parsed.params).await {
        Ok(result) => jsonrpc_result(parsed.id, result),
        Err(error) => jsonrpc_error(parsed.id, jsonrpc_error_code(&error), &error.message),
    };
    Some(response)
}

async fn dispatch_ws_method(
    state: &Arc<AppState>,
    method: &str,
    params: Option<Value>,
) -> Result<Value, ApiError> {
    match method {
        "status" => {
            Ok(serde_json::to_value(get_status(State(state.clone())).await.0).unwrap_or_default())
        }
        "contacts" => contacts_value(state).await,
        "sessions" => Ok(sessions_value(state).await),
        "listen_list" => {
            Ok(serde_json::to_value(state.wechat.get_listen_list().await).unwrap_or_default())
        }
        "send" => {
            let req: SendRequest = serde_json::from_value(params.unwrap_or(Value::Null))
                .map_err(|e| ApiError::internal(format!("参数解析失败: {e}")))?;
            Ok(serde_json::to_value(send_message_inner(state, req).await?).unwrap_or_default())
        }
        "send_image" => {
            let req: SendImageRequest = serde_json::from_value(params.unwrap_or(Value::Null))
                .map_err(|e| ApiError::internal(format!("参数解析失败: {e}")))?;
            Ok(serde_json::to_value(send_image_inner(state, req).await?).unwrap_or_default())
        }
        "chat" => {
            let req: ChatRequest = serde_json::from_value(params.unwrap_or(Value::Null))
                .map_err(|e| ApiError::internal(format!("参数解析失败: {e}")))?;
            Ok(serde_json::to_value(chat_with_inner(state, req).await?).unwrap_or_default())
        }
        "listen" => {
            let req: ListenRequest = serde_json::from_value(params.unwrap_or(Value::Null))
                .map_err(|e| ApiError::internal(format!("参数解析失败: {e}")))?;
            Ok(serde_json::to_value(add_listen_inner(state, req).await?).unwrap_or_default())
        }
        "unlisten" => {
            let req: ListenRequest = serde_json::from_value(params.unwrap_or(Value::Null))
                .map_err(|e| ApiError::internal(format!("参数解析失败: {e}")))?;
            Ok(serde_json::to_value(remove_listen_inner(state, req).await).unwrap_or_default())
        }
        "screenshot" => {
            let png_data = tokio::task::spawn_blocking(crate::input::capture_screenshot)
                .await
                .map_err(|e| ApiError::internal(format!("截屏任务失败: {e}")))?
                .map_err(|e| ApiError::internal(format!("截屏失败: {e}")))?;
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&png_data);
            Ok(serde_json::json!({ "image": b64, "format": "png", "size": png_data.len() }))
        }
        "command" => {
            let cmd = params
                .as_ref()
                .and_then(|value| value.get("cmd"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            Ok(serde_json::json!({
                "ok": true,
                "result": exec_command_text(state, cmd).await,
            }))
        }
        _ => Err(ApiError::internal(format!("未知方法: {method}"))),
    }
}

fn jsonrpc_result(id: Option<Value>, result: Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "result": result,
        "id": id,
    })
    .to_string()
}

fn jsonrpc_error(id: Option<Value>, code: i32, message: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "error": { "code": code, "message": message },
        "id": id,
    })
    .to_string()
}

fn jsonrpc_error_code(error: &ApiError) -> i32 {
    if error.message.starts_with("未知方法:") {
        -32601
    } else {
        error.status.as_u16() as i32
    }
}

// =====================================================================
// POST /command — 通用命令执行 (微信互通)
// =====================================================================

#[derive(Deserialize)]
struct CommandReq {
    cmd: String,
}

#[utoipa::path(
    post, path = "/command",
    tag = "Actions",
    summary = "执行命令 (通用命令接口)",
    description = "通用命令执行接口, 支持以下命令:\n\n| 命令 | 说明 |\n|------|------|\n| `status` | 获取详细运行状态 |\n| `send <收件人> <内容>` | 发送文本消息 |\n| `listen <联系人/群名>` | 添加监听并持久化到配置文件 |\n| `unlisten <联系人/群名>` | 移除监听并更新配置文件 |\n| `reload` | 重载配置文件, 自动 diff 监听列表和参数变更 |\n| `atmode` | 切换仅@模式 (通过 WebSocket 广播控制指令) |\n\n返回格式: `{ \"ok\": true, \"result\": \"...\" }`",
    security(("bearer" = [])),
    request_body = CommandReqSchema,
    responses(
        (status = 200, description = "命令执行结果 `{ \"ok\": true, \"result\": \"...\" }`"),
        (status = 401, description = "未授权 — Token 缺失或不匹配")
    )
)]
async fn exec_command(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CommandReq>,
) -> impl IntoResponse {
    let cmd = req.cmd.trim();
    info!("🎮 收到远程命令: {cmd}");
    let result = exec_command_text(&state, cmd).await;
    info!("🎮 命令结果: {result}");
    Json(serde_json::json!({ "ok": true, "result": result }))
}

async fn exec_command_text(state: &Arc<AppState>, cmd: &str) -> String {
    let result = match cmd {
        "status" => {
            let status = state.wechat.check_status().await;
            let runtime = state.runtime.snapshot().await;
            let input = state.input_metrics.snapshot();
            let listen_list = state.wechat.get_listen_list().await;
            let db_status = if state.db.get().is_some() {
                "可用"
            } else {
                "不可用"
            };
            let contacts = if let Some(d) = state.db.get() {
                d.get_contacts().await.len()
            } else {
                0
            };
            let uptime = state.start_time.elapsed().as_secs();
            let h = uptime / 3600;
            let m = (uptime % 3600) / 60;
            format!(
                "📊 运行时: {}{}\n📊 微信: {status}\n📊 数据库: {db_status} | 联系人: {contacts}\n📊 输入: queue={} last={}ms max={}ms fail={}\n📊 监听: {} 个 {:?}\n📊 运行: {h}h{m}m | v{}",
                runtime.state,
                runtime
                    .reason
                    .as_deref()
                    .map(|reason| format!(" ({reason})"))
                    .unwrap_or_default(),
                input.queue_depth,
                input.last_command_ms,
                input.max_command_ms,
                input.total_failures,
                listen_list.len(),
                listen_list,
                env!("CARGO_PKG_VERSION")
            )
        }
        "atmode" => {
            let _ = state.tx.send(WxEvent::Control {
                cmd: "toggle_at_mode".to_string(),
            });
            "📢 已发送仅@模式切换指令".to_string()
        }
        "reload" => exec_reload(&state).await,
        _ if cmd.starts_with("listen ") => {
            let who = cmd.strip_prefix("listen ").unwrap().trim();
            if who.is_empty() {
                "❌ 用法: listen <联系人/群名>".to_string()
            } else {
                exec_listen(&state, who).await
            }
        }
        _ if cmd.starts_with("unlisten ") => {
            let who = cmd.strip_prefix("unlisten ").unwrap().trim();
            if who.is_empty() {
                "❌ 用法: unlisten <联系人/群名>".to_string()
            } else {
                exec_unlisten(&state, who).await
            }
        }
        _ if cmd.starts_with("send ") => {
            let rest = cmd.strip_prefix("send ").unwrap().trim();
            if let Some((to, text)) = rest.split_once(' ') {
                exec_send(&state, to.trim(), text.trim()).await
            } else {
                "❌ 用法: send <收件人> <内容>".to_string()
            }
        }
        _ => format!("❓ 未知命令: {cmd}"),
    };
    result
}

/// 执行 reload 命令
async fn exec_reload(state: &AppState) -> String {
    let path = match &state.config_path {
        Some(p) => p,
        None => return "⚠️ 未找到配置文件路径".to_string(),
    };
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => return format!("⚠️ 读取配置失败: {e}"),
    };
    let new_config: crate::AppConfig = match toml::from_str(&content) {
        Ok(c) => c,
        Err(e) => return format!("⚠️ 配置解析失败: {e}"),
    };

    let mut lines = Vec::new();

    // 更新 at_delay_ms
    let old = state.wechat.get_at_delay_ms();
    let new = new_config.timing.at_delay_ms;
    if old != new {
        state.wechat.set_at_delay_ms(new);
        lines.push(format!("⚙️ at_delay_ms: {old} → {new}"));
    }

    // Diff listen 列表
    let current = state.wechat.get_listen_list().await;
    let new_list = new_config.listen.auto;
    let to_add: Vec<_> = new_list
        .iter()
        .filter(|n| !current.contains(n))
        .cloned()
        .collect();
    let to_remove: Vec<_> = current
        .iter()
        .filter(|n| !new_list.contains(n))
        .cloned()
        .collect();

    for who in &to_remove {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if enqueue_input_command(
            &state.input_tx,
            &state.input_metrics,
            InputCommand::RemoveListen {
                who: who.clone(),
                reply: reply_tx,
            },
        )
        .await
        .is_ok()
        {
            let _ = reply_rx.await;
        }
        lines.push(format!("👂 移除监听: {who}"));
    }
    for who in &to_add {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if enqueue_input_command(
            &state.input_tx,
            &state.input_metrics,
            InputCommand::AddListen {
                who: who.clone(),
                reply: reply_tx,
            },
        )
        .await
        .is_ok()
        {
            match reply_rx.await {
                Ok(Ok(true)) => lines.push(format!("✅ 添加监听: {who}")),
                _ => lines.push(format!("⚠️ 添加失败: {who}")),
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }

    if lines.is_empty() {
        "⚙️ 配置已重载 (无变化)".to_string()
    } else {
        lines.push("⚙️ 配置已重载".to_string());
        lines.join("\n")
    }
}

/// 执行 listen 命令
async fn exec_listen(state: &AppState, who: &str) -> String {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if enqueue_input_command(
        &state.input_tx,
        &state.input_metrics,
        InputCommand::AddListen {
            who: who.to_string(),
            reply: reply_tx,
        },
    )
    .await
    .is_err()
    {
        return "⚠️ InputEngine 不可用".to_string();
    }
    match reply_rx.await {
        Ok(Ok(true)) => {
            // 持久化
            if let Some(ref path) = state.config_path {
                let mut list = state.wechat.get_listen_list().await;
                if !list.contains(&who.to_string()) {
                    list.push(who.to_string());
                }
                crate::save_listen_list(path, &list);
            }
            format!("✅ 监听已添加: {who}")
        }
        Ok(Ok(false)) => format!("⚠️ 添加失败: {who}"),
        Ok(Err(e)) => format!("⚠️ 错误: {e}"),
        Err(_) => "⚠️ actor 响应通道已关闭".to_string(),
    }
}

/// 执行 unlisten 命令
async fn exec_unlisten(state: &AppState, who: &str) -> String {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if enqueue_input_command(
        &state.input_tx,
        &state.input_metrics,
        InputCommand::RemoveListen {
            who: who.to_string(),
            reply: reply_tx,
        },
    )
    .await
    .is_err()
    {
        return "⚠️ InputEngine 不可用".to_string();
    }
    match reply_rx.await {
        Ok(true) => {
            // 持久化
            if let Some(ref path) = state.config_path {
                let mut list = state.wechat.get_listen_list().await;
                list.retain(|n| n != who);
                crate::save_listen_list(path, &list);
            }
            format!("✅ 监听已移除: {who}")
        }
        Ok(false) => format!("⚠️ 未找到监听: {who}"),
        Err(_) => "⚠️ actor 响应通道已关闭".to_string(),
    }
}

/// 执行 send 命令
async fn exec_send(state: &AppState, to: &str, text: &str) -> String {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let sent_rx = state.db.get().map(|db| db.subscribe_sent());
    if enqueue_input_command(
        &state.input_tx,
        &state.input_metrics,
        InputCommand::SendMessage {
            to: to.to_string(),
            text: text.to_string(),
            at: vec![],
            skip_verify: true,
            reply: reply_tx,
        },
    )
    .await
    .is_err()
    {
        return "⚠️ InputEngine 不可用".to_string();
    }
    match reply_rx.await {
        Ok(Ok((true, _, msg))) => {
            let verified = if let (Some(db), Some(rx)) = (state.db.get(), sent_rx) {
                db.verify_sent(text, rx).await.unwrap_or(false)
            } else {
                state.wechat.verify_sent_after_send(to, text).await
            };
            format!("✅ {msg} | verified={verified}")
        }
        Ok(Ok((false, _, msg))) => format!("⚠️ {msg}"),
        Ok(Err(e)) => format!("⚠️ 发送失败: {e}"),
        Err(_) => "⚠️ actor 响应通道已关闭".to_string(),
    }
}
