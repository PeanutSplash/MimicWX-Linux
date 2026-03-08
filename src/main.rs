//! MimicWX-Linux: 微信自动化框架
//!
//! 架构:
//! - atspi: AT-SPI2 底层原语 (D-Bus 通信) — 仅用于发送消息
//! - wechat: 微信业务逻辑 (控件查找、消息发送/验证、会话管理)
//! - chatwnd: 独立聊天窗口 (借鉴 wxauto ChatWnd)
//! - input: X11 XTEST 输入注入
//! - db: 数据库监听 (SQLCipher 解密 + fanotify WAL 监听)
//! - api: HTTP/WebSocket API

mod api;
mod atspi;
mod chatwnd;
mod db;
mod events;
mod input;
mod keyscan;
mod node_handle;
mod ports;
mod runtime;
mod wechat;

use anyhow::Result;
use events::WxEvent;
use runtime::{RuntimeManager, RuntimeState};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

// =====================================================================
// 配置文件
// =====================================================================

#[derive(Debug, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    api: ApiConfig,
    #[serde(default)]
    listen: ListenConfig,
    #[serde(default)]
    timing: TimingConfig,
}

#[derive(Debug, Deserialize, Default)]
pub struct ApiConfig {
    /// API 认证 Token (留空或不配置则不启用认证)
    #[serde(default)]
    token: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ListenConfig {
    /// 启动后自动弹出独立窗口并监听的对象
    #[serde(default)]
    pub auto: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct TimingConfig {
    /// @ 输入流程中每步的等待时间 (毫秒)
    #[serde(default = "default_at_delay")]
    pub at_delay_ms: u64,
}

impl Default for TimingConfig {
    fn default() -> Self {
        Self { at_delay_ms: 300 }
    }
}

fn default_at_delay() -> u64 {
    300
}

/// 默认配置文件模板
const DEFAULT_CONFIG_TEMPLATE: &str = r#"# MimicWX-Linux 配置文件

[api]
# API 认证 Token
# 留空则每次启动时自动随机生成并回写到此文件; 填写后使用固定值
# 请求方式: Header "Authorization: Bearer <token>" 或 Query "?token=<token>"
token = ""

[listen]
# 启动后自动弹出独立窗口并监听的对象
# 填入联系人名称或群名称 (与微信显示名一致)
# 示例: auto = ["NIUNIU","文件传输助手","zzz"]
auto = []

[timing]
# @ 输入流程中每步的等待时间 (毫秒)
# 降低可加快速度, 但太低可能导致微信选择器来不及响应
# 默认 300, 建议范围 150~500
at_delay_ms = 300
"#;

/// 加载配置文件 (搜索多个路径, 不存在则自动创建)
/// 返回 (配置, 配置文件路径)
fn load_config() -> (AppConfig, Option<PathBuf>) {
    let search_paths = [
        PathBuf::from("./config.toml"),
        PathBuf::from("/home/wechat/mimicwx-linux/config.toml"),
        PathBuf::from("/etc/mimicwx/config.toml"),
    ];
    for path in &search_paths {
        if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(content) => match toml::from_str::<AppConfig>(&content) {
                    Ok(config) => {
                        info!("⚙️ 配置文件已加载: {}", path.display());
                        return (config, Some(path.clone()));
                    }
                    Err(e) => {
                        warn!("⚠️ 配置文件解析失败: {} - {}", path.display(), e);
                    }
                },
                Err(e) => {
                    warn!("⚠️ 配置文件读取失败: {} - {}", path.display(), e);
                }
            }
        }
    }
    // 自动创建默认配置文件
    let create_path = &search_paths[0];
    match std::fs::write(create_path, DEFAULT_CONFIG_TEMPLATE) {
        Ok(_) => info!("⚙️ 已自动创建配置文件: {}", create_path.display()),
        Err(e) => warn!("⚠️ 无法创建配置文件: {} - {}", create_path.display(), e),
    }
    (AppConfig::default(), Some(create_path.clone()))
}

/// 将自动生成的 token 回写到 config.toml
pub fn save_token(config_path: &std::path::Path, token: &str) {
    let content = match std::fs::read_to_string(config_path) {
        Ok(c) => c,
        Err(e) => {
            warn!("⚠️ 无法读取配置文件: {e}");
            return;
        }
    };
    let mut new_lines: Vec<String> = Vec::new();
    let mut found = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('#') && trimmed.starts_with("token") && trimmed.contains('=') {
            new_lines.push(format!("token = \"{}\"", token));
            found = true;
        } else {
            new_lines.push(line.to_string());
        }
    }
    let new_content = if found {
        new_lines.join("\n")
    } else {
        content.replace("[api]", &format!("[api]\ntoken = \"{}\"", token))
    };
    match std::fs::write(config_path, new_content) {
        Ok(_) => info!("⚙️ Token 已保存到 {}", config_path.display()),
        Err(e) => warn!("⚠️ 保存 Token 失败: {e}"),
    }
}

/// 保存监听列表到 config.toml (仅替换 auto = [...] 行, 保留注释和格式)
pub fn save_listen_list(config_path: &std::path::Path, listen_list: &[String]) {
    let content = match std::fs::read_to_string(config_path) {
        Ok(c) => c,
        Err(e) => {
            warn!("⚠️ 无法读取配置文件: {e}");
            return;
        }
    };

    // 构造新的 auto 行 (横排格式, 与用户原始风格一致)
    let new_auto = if listen_list.is_empty() {
        "auto = []".to_string()
    } else {
        let items: Vec<_> = listen_list.iter().map(|s| format!("\"{}\"", s)).collect();
        format!("auto = [{}]", items.join(","))
    };

    // 逐行扫描, 找到非注释的 auto = [...] 行并替换
    // (跳过 # 开头的注释行, 避免误匹配 "# 示例: auto = [...]")
    let mut new_lines: Vec<String> = Vec::new();
    let mut found = false;
    let mut skip_continuation = false; // 跨行数组: 跳过后续行直到 ]
    for line in content.lines() {
        if skip_continuation {
            if line.contains(']') {
                skip_continuation = false;
            }
            continue; // 跳过跨行数组的中间行
        }
        let trimmed = line.trim();
        if !trimmed.starts_with('#') && trimmed.starts_with("auto") && trimmed.contains('=') {
            // 这是真正的 auto = [...] 行
            if trimmed.contains('[') && !trimmed.contains(']') {
                // 跨行数组: auto = [\n  "a",\n  "b",\n]
                skip_continuation = true;
            }
            new_lines.push(new_auto.clone());
            found = true;
        } else {
            new_lines.push(line.to_string());
        }
    }
    let new_content = if found {
        new_lines.join("\n")
    } else {
        // 没有 auto 行, 在 [listen] 段后追加
        content.replace("[listen]", &format!("[listen]\n{}", new_auto))
    };

    match std::fs::write(config_path, new_content) {
        Ok(_) => info!("⚙️ 监听列表已保存到 {}", config_path.display()),
        Err(e) => warn!("⚠️ 保存配置失败: {e}"),
    }
}

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    // 日志 (with_ansi(true) 强制启用 ANSI 颜色, 即使 stderr 重定向到文件)
    tracing_subscriber::fmt()
        .with_ansi(true)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mimicwx=debug,tower_http=info".into()),
        )
        .init();

    info!("🚀 MimicWX-Linux v{} 启动中...", env!("CARGO_PKG_VERSION"));

    let runtime = Arc::new(RuntimeManager::new(RuntimeState::Booting));

    // ① 加载配置文件, 环境变量覆盖
    let (mut config, config_path) = load_config();

    // 环境变量覆盖: MIMICWX_TOKEN, MIMICWX_LISTEN, MIMICWX_AT_DELAY_MS
    if let Ok(v) = std::env::var("MIMICWX_TOKEN") {
        config.api.token = Some(v);
    }
    if let Ok(v) = std::env::var("MIMICWX_LISTEN") {
        config.listen.auto = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    }
    if let Ok(v) = std::env::var("MIMICWX_AT_DELAY_MS") {
        if let Ok(ms) = v.parse::<u64>() {
            config.timing.at_delay_ms = ms;
        }
    }

    if !config.listen.auto.is_empty() {
        debug!("📋 自动监听列表: {:?}", config.listen.auto);
    }

    // ② AT-SPI2 连接 (仍用于发送消息, 带重试)
    let atspi = loop {
        match atspi::AtSpi::connect().await {
            Ok(a) => {
                debug!("AT-SPI2 连接就绪");
                break Arc::new(a);
            }
            Err(e) => {
                warn!("⚠️ AT-SPI2 连接失败: {}, 5秒后重试...", e);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    };

    // ③ X11 XTEST 输入引擎 (仅发送消息需要, 非必须)
    let engine = match input::InputEngine::new() {
        Ok(e) => {
            debug!("X11 XTEST 输入引擎就绪");
            Some(e)
        }
        Err(e) => {
            warn!("⚠️ X11 输入引擎不可用 (发送消息功能受限): {}", e);
            None
        }
    };
    if engine.is_some() {
        runtime.transition_to(RuntimeState::DesktopReady).await;
    } else {
        runtime
            .degrade("X11 输入引擎不可用，发送消息功能受限")
            .await;
    }

    // ④ WeChat 实例化 (AT-SPI 部分, 用于发送)
    let wechat = Arc::new(wechat::WeChat::new(
        atspi.clone(),
        config.timing.at_delay_ms,
    ));

    // ⑤ 广播通道 + InputEngine Actor (提前启动, 使 API 在登录等待阶段可用)
    let (tx, _) = tokio::sync::broadcast::channel::<WxEvent>(128);

    let (input_tx, input_rx) = tokio::sync::mpsc::channel::<api::InputCommand>(32);
    let input_metrics = Arc::new(api::InputMetrics::default());

    if let Some(eng) = engine {
        api::spawn_input_actor(eng, wechat.clone(), input_metrics.clone(), input_rx);
    } else {
        debug!("X11 输入引擎不可用, InputEngine actor 未启动");
    }

    // ⑥ 创建 AppState (db 使用 OnceLock, 稍后在 DB 就绪时设置)
    let db_lock: Arc<std::sync::OnceLock<Arc<db::DbManager>>> =
        Arc::new(std::sync::OnceLock::new());

    let state = Arc::new(api::AppState {
        wechat: wechat.clone(),
        atspi: atspi.clone(),
        runtime: runtime.clone(),
        input_metrics: input_metrics.clone(),
        input_tx: input_tx.clone(),
        tx: tx.clone(),
        db: db_lock.clone(),
        api_token: {
            let t = config.api.token.filter(|t| !t.is_empty());
            if t.is_some() {
                t
            } else {
                let seed = format!(
                    "{}-{}-{:?}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos(),
                    std::thread::current().id(),
                );
                let token = format!("{:x}", md5::compute(&seed));
                warn!("🔑 未配置 Token, 已自动生成: {token}");
                // 回写到配置文件
                if let Some(ref path) = config_path {
                    save_token(path, &token);
                }
                Some(token)
            }
        },
        start_time: std::time::Instant::now(),
        config_path: config_path.clone(),
    });

    // ⑦ 退出码 + 关闭信号
    let exit_code = Arc::new(AtomicI32::new(0));
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);

    // ⑧ 启动 API 服务 (提前启动, /screenshot 和 /status 在登录等待阶段即可用)
    let app = api::build_router(state.clone());
    let addr = std::env::var("MIMICWX_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8899".to_string());
    info!("🌐 API 服务启动: http://{addr} (文档: /docs, WebSocket: /ws)");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    {
        let mut api_shutdown_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = api_shutdown_rx.recv().await;
                    info!("🛑 API 服务正在关闭...");
                })
                .await
                .ok();
        });
    }

    // ⑨ RuntimeState 变更桥接到 WxEvent 广播
    {
        let mut runtime_rx = runtime.subscribe();
        let runtime_tx = tx.clone();
        tokio::spawn(async move {
            while let Ok(event) = runtime_rx.recv().await {
                let _ = runtime_tx.send(WxEvent::StatusChange {
                    from: event.from,
                    to: event.to,
                });
            }
        });
    }

    // ⑩ AT-SPI2 健康检查心跳 (每 30s 检查连接, 连续 3 次异常自动重连)
    {
        let hb_atspi = atspi.clone();
        let mut hb_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            let mut fail_count: u32 = 0;
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            interval.tick().await; // 跳过首次立即触发

            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = hb_shutdown.recv() => {
                        debug!("💓 AT-SPI2 心跳停止");
                        break;
                    }
                }

                if let Some(registry) = atspi::AtSpi::registry() {
                    let count = hb_atspi.child_count(&registry).await;
                    if count > 0 {
                        if fail_count > 0 {
                            info!("AT-SPI2 连接恢复 ({count} 个应用)");
                        }
                        fail_count = 0;
                    } else {
                        fail_count += 1;
                        debug!("AT-SPI2 心跳: Registry 返回 0 个应用 (连续 {fail_count} 次)");
                        if fail_count >= 3 {
                            warn!("AT-SPI2 连续 {fail_count} 次心跳异常, 尝试重连...");
                            if hb_atspi.reconnect().await {
                                fail_count = 0;
                            } else {
                                warn!("AT-SPI2 重连失败, 30s 后再试");
                            }
                        }
                    }
                }
            }
        });
    }

    // ⑪ 控制台命令读取器 (stdin)
    {
        let console_exit = exit_code.clone();
        let console_shutdown = shutdown_tx.clone();
        let console_wechat = wechat.clone();
        let console_runtime = runtime.clone();
        let console_metrics = input_metrics.clone();
        let console_tx = tx.clone();
        let console_input_tx = input_tx.clone();
        let console_config_path = config_path.clone();
        let console_db = db_lock.clone();
        tokio::spawn(async move {
            console_loop(
                console_exit,
                console_shutdown,
                console_runtime,
                console_metrics,
                console_wechat,
                console_db,
                console_tx,
                console_input_tx,
                console_config_path,
            )
            .await;
        });
    }

    // ⑫ 等待微信就绪 (API 已在后台运行, /screenshot 可用于扫码)
    let mut attempts = 0;
    let mut login_prompted = false;
    let mut wechat_ready_seen = false;
    let mut last_qr_content: Option<String> = None;
    let mut qr_lines_printed: usize = 0;
    loop {
        let status = wechat.check_status().await;
        match status {
            wechat::WeChatStatus::LoggedIn => {
                // 清除终端上的二维码
                if qr_lines_printed > 0 {
                    clear_terminal_lines(qr_lines_printed);
                }
                if !runtime.is_degraded().await {
                    runtime.transition_to(RuntimeState::WeChatReady).await;
                }
                info!("✅ 微信已登录");
                break;
            }
            wechat::WeChatStatus::NotRunning if attempts < 30 => {
                debug!("⏳ 等待微信启动... ({}/30)", attempts + 1);
                if attempts % 5 == 4 {
                    wechat.try_reconnect().await;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                attempts += 1;
            }
            wechat::WeChatStatus::WaitingForLogin => {
                if !wechat_ready_seen {
                    wechat_ready_seen = true;
                    if !runtime.is_degraded().await {
                        runtime.transition_to(RuntimeState::WeChatReady).await;
                    }
                }
                if !runtime.is_degraded().await {
                    runtime.transition_to(RuntimeState::LoginWaiting).await;
                }
                if !login_prompted {
                    info!("📱 等待扫码登录... (二维码: 终端 / http://<HOST>:8899/screenshot)");
                    login_prompted = true;
                }

                // 尝试从截屏中检测二维码并打印到终端
                if let Some(qr_content) =
                    tokio::task::spawn_blocking(input::detect_qr_from_screenshot)
                        .await
                        .ok()
                        .flatten()
                {
                    if last_qr_content.as_ref() != Some(&qr_content) {
                        // 二维码内容变化, 清除旧的并打印新的
                        if qr_lines_printed > 0 {
                            clear_terminal_lines(qr_lines_printed);
                        }
                        if let Some((rendered, lines)) = input::render_qr_to_terminal(&qr_content) {
                            eprintln!("\n{rendered}\n  📱 请用微信扫描上方二维码登录\n");
                            qr_lines_printed = lines + 3; // +3: 空行 + 二维码 + 提示行
                        }
                        last_qr_content = Some(qr_content);
                    }
                }

                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
            wechat::WeChatStatus::NotRunning => {
                runtime.degrade("微信未在预期时间内启动或窗口未就绪").await;
                break;
            }
        }
    }

    // ⑬ 扫描微信进程内存, 构建数据库目录与按库密钥注册表
    let db_manager: Option<Arc<db::DbManager>> = match find_db_dir() {
        Some(dir) => {
            debug!("开始解析数据库密钥: {}", dir.display());
            match tokio::task::spawn_blocking(move || keyscan::resolve_catalog(dir)).await {
                Ok(Ok(resolved)) => {
                    debug!(
                        "数据库密钥解析完成: {}/{} 个 DB, {} 个 hex 模式, {} 个进程",
                        resolved.summary.resolved_keys,
                        resolved.summary.db_files,
                        resolved.summary.hex_patterns,
                        resolved.summary.process_count
                    );
                    match db::DbManager::new(resolved.catalog, resolved.registry) {
                        Ok(mgr) => {
                            let mgr = Arc::new(mgr);
                            match mgr.validate_required() {
                                Ok(validated) => {
                                    info!("🔓 关键数据库验证通过: {}", validated.join(", "));
                                    if !runtime.is_degraded().await {
                                        runtime.transition_to(RuntimeState::KeyReady).await;
                                    }

                                    // 等待微信同步数据库后再加载联系人 (刚登录时表不完整)
                                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                    if let Err(e) = mgr.refresh_contacts().await {
                                        warn!("⚠️ 联系人加载失败: {}", e);
                                        if !runtime.is_degraded().await {
                                            runtime.degrade(format!("联系人加载失败: {e}")).await;
                                        }
                                        None
                                    } else {
                                        if !runtime.is_degraded().await {
                                            runtime.transition_to(RuntimeState::DbReady).await;
                                        }
                                        if let Err(e) = mgr.prime_session_state().await {
                                            warn!("⚠️ Session 基线初始化失败: {}", e);
                                            if !runtime.is_degraded().await {
                                                runtime
                                                    .degrade(format!("Session 基线初始化失败: {e}"))
                                                    .await;
                                            }
                                            None
                                        } else {
                                            if !runtime.is_degraded().await {
                                                runtime.transition_to(RuntimeState::Serving).await;
                                            }
                                            Some(mgr)
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("⚠️ 关键数据库验证失败: {}", e);
                                    if !runtime.is_degraded().await {
                                        runtime.degrade(format!("关键数据库验证失败: {e}")).await;
                                    }
                                    None
                                }
                            }
                        }
                        Err(e) => {
                            warn!("⚠️ DbManager 初始化失败: {}", e);
                            if !runtime.is_degraded().await {
                                runtime.degrade(format!("DbManager 初始化失败: {e}")).await;
                            }
                            None
                        }
                    }
                }
                Ok(Err(e)) => {
                    warn!("⚠️ 数据库密钥解析失败: {}", e);
                    if !runtime.is_degraded().await {
                        runtime.degrade(format!("数据库密钥解析失败: {e}")).await;
                    }
                    None
                }
                Err(e) => {
                    warn!("⚠️ 密钥扫描任务异常: {}", e);
                    if !runtime.is_degraded().await {
                        runtime.degrade(format!("密钥扫描任务异常: {e}")).await;
                    }
                    None
                }
            }
        }
        None => {
            warn!("⚠️ 未找到微信数据库目录, 数据库监听不可用");
            if !runtime.is_degraded().await {
                runtime.degrade("未找到微信数据库目录").await;
            }
            None
        }
    };

    // 将 DbManager 设置到 OnceLock (API 层立即可见)
    if let Some(ref mgr) = db_manager {
        let _ = db_lock.set(mgr.clone());
    }

    // ⑭ 后台数据库消息监听任务
    if let Some(db) = db_manager {
        let listen_tx = tx.clone();

        // ⑭-a) 联系人定时刷新 (每 5 分钟, 新好友/群不用重启就有名字)
        {
            let refresh_db = Arc::clone(&db);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
                interval.tick().await; // 跳过首次 (启动时已加载)
                loop {
                    interval.tick().await;
                    match refresh_db.refresh_contacts().await {
                        Ok(n) => debug!("👥 联系人定时刷新完成: {} 条", n),
                        Err(e) => warn!("⚠️ 联系人定时刷新失败: {}", e),
                    }
                }
            });
        }

        // 启动 session.db 变化监听 (mtime 轮询)
        let mut wal_rx = db.spawn_wal_watcher();

        tokio::spawn(async move {
            info!("👂 Session 消息监听启动 (SessionTable 驱动)");

            loop {
                match wal_rx.recv().await {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        error!("❌ Session 监听通道关闭");
                        break;
                    }
                }

                // 基于 SessionTable 拉取新消息
                match db.get_new_messages().await {
                    Ok(msgs) => {
                        for m in &msgs {
                            let _ = listen_tx.send(WxEvent::Message(m.clone()));
                        }
                    }
                    Err(e) => {
                        tracing::debug!("📭 消息查询: {}", e);
                    }
                }
            }
        });
    } else {
        warn!("⚠️ 数据库密钥不可用, 消息监听功能未启动");
    }

    // ⑮ 自动监听任务 (配置文件中的 auto listen 列表)
    if !config.listen.auto.is_empty() {
        let auto_targets = config.listen.auto.clone();
        let auto_input_tx = input_tx.clone();
        let auto_metrics = input_metrics.clone();
        tokio::spawn(async move {
            // 等待 API 服务就绪 + 微信窗口稳定
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            info!("📋 开始自动添加监听 ({} 个目标)...", auto_targets.len());

            for target in &auto_targets {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if api::enqueue_input_command(
                    &auto_input_tx,
                    &auto_metrics,
                    api::InputCommand::AddListen {
                        who: target.clone(),
                        reply: reply_tx,
                    },
                )
                .await
                .is_err()
                {
                    warn!("⚠️ InputEngine actor 已停止, 无法自动添加监听");
                    break;
                }
                match reply_rx.await {
                    Ok(Ok(true)) => info!("✅ 自动监听已添加: {}", target),
                    Ok(Ok(false)) => warn!("⚠️ 自动监听添加失败: {}", target),
                    Ok(Err(e)) => warn!("⚠️ 自动监听错误: {} - {}", target, e),
                    Err(_) => warn!("⚠️ actor 响应通道已关闭"),
                }
                // 每个目标间隔 3 秒, 给微信窗口时间稳定
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }

            info!("📋 自动监听配置完成");
        });
    }

    // ⑯ 等待关闭信号 (Ctrl+C 或 /restart /stop 命令)
    info!("💡 控制台命令: /restart /stop /status /refresh /help");

    let mut final_shutdown_rx = shutdown_tx.subscribe();
    tokio::select! {
        _ = final_shutdown_rx.recv() => {
            info!("🛑 收到关闭信号...");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("🛑 收到 Ctrl+C, 停止服务...");
        }
    }

    // 通知所有后台任务停止
    let _ = shutdown_tx.send(());

    let code = exit_code.load(Ordering::Relaxed);
    if code == 42 {
        info!("🔄 MimicWX 准备重启...");
    } else {
        info!("👋 MimicWX 已停止");
    }
    std::process::exit(code);
}

/// 查找微信数据库目录
///
/// WeChat Linux 数据库路径 (实际):
/// ~/Documents/xwechat_files/wxid_xxx/db_storage
/// 当存在多个 wxid 时 (换账号), 选择最近修改的目录
fn find_db_dir() -> Option<PathBuf> {
    let mut candidates: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();

    // 收集所有可能的 xwechat_files 路径 (用 HashSet 去重)
    let mut search_dirs = std::collections::HashSet::new();
    let home = dirs_or_home();
    // 新版路径: ~/Documents/xwechat_files
    search_dirs.insert(PathBuf::from("/home/wechat/Documents/xwechat_files"));
    search_dirs.insert(home.join("Documents/xwechat_files"));
    // 新版路径 (部分版本直接放在 ~/ 下): ~/xwechat_files
    search_dirs.insert(PathBuf::from("/home/wechat/xwechat_files"));
    search_dirs.insert(home.join("xwechat_files"));
    // Fallback: 扫描 /home 下所有用户
    if let Ok(homes) = std::fs::read_dir("/home") {
        for h in homes.flatten() {
            search_dirs.insert(h.path().join("Documents/xwechat_files"));
            search_dirs.insert(h.path().join("xwechat_files"));
        }
    }

    for xwechat_dir in &search_dirs {
        if let Ok(entries) = std::fs::read_dir(xwechat_dir) {
            for entry in entries.flatten() {
                let db_storage = entry.path().join("db_storage");
                if db_storage.exists() {
                    let msg_dir = db_storage.join("message");
                    let mtime = msg_dir
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    debug!("📂 候选: {} (mtime={:?})", db_storage.display(), mtime);
                    candidates.push((db_storage, mtime));
                }
            }
        }
    }

    // 选择最新修改的目录 (活跃账号)
    if !candidates.is_empty() {
        candidates.sort_by(|a, b| b.1.cmp(&a.1));
        let chosen = &candidates[0].0;
        if candidates.len() > 1 {
            debug!(
                "发现 {} 个账号目录, 选择最新的: {}",
                candidates.len(),
                chosen.display()
            );
        } else {
            debug!("数据库目录: {}", chosen.display());
        }
        return Some(chosen.clone());
    }

    // 也尝试旧路径格式
    let old_path = PathBuf::from("/home/wechat/.local/share/weixin/data/db_storage");
    if old_path.exists() {
        debug!("数据库目录 (旧格式): {}", old_path.display());
        return Some(old_path);
    }

    None
}

/// 清除终端上方 n 行 (ANSI escape: 上移 + 清行)
fn clear_terminal_lines(n: usize) {
    use std::io::Write;
    let mut out = std::io::stderr();
    for _ in 0..n {
        let _ = write!(out, "\x1b[A\x1b[2K");
    }
    let _ = out.flush();
}

fn dirs_or_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root"))
}

// =====================================================================
// 交互式控制台 (raw terminal mode + 行编辑 + 历史命令)
// =====================================================================

/// Raw mode guard — Drop 时自动恢复终端
struct RawModeGuard(libc::termios);

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.0);
        }
        let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\r\n");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
}

/// 启用 raw input mode (关闭行缓冲+回显, 保留输出处理和信号)
fn enable_raw_mode() -> Option<RawModeGuard> {
    unsafe {
        let mut orig: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut orig) != 0 {
            return None;
        }
        let mut raw = orig;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw) != 0 {
            return None;
        }
        Some(RawModeGuard(orig))
    }
}

/// 重绘提示行
fn redraw_prompt(line: &str, cursor: usize) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\r\x1b[K> {}", line);
    let move_back = line[cursor..].chars().count();
    if move_back > 0 {
        let _ = write!(out, "\x1b[{}D", move_back);
    }
    let _ = out.flush();
}

async fn handle_command(
    cmd: &str,
    exit_code: &Arc<AtomicI32>,
    shutdown_tx: &tokio::sync::broadcast::Sender<()>,
    runtime: &Arc<RuntimeManager>,
    input_metrics: &Arc<api::InputMetrics>,
    wechat: &Arc<wechat::WeChat>,
    db: &std::sync::OnceLock<Arc<db::DbManager>>,
    broadcast_tx: &tokio::sync::broadcast::Sender<WxEvent>,
    input_tx: &tokio::sync::mpsc::Sender<api::InputCommand>,
    config_path: &Option<PathBuf>,
) -> bool {
    match cmd {
        "/restart" => {
            info!("🔄 收到 /restart 命令, 准备重启...");
            exit_code.store(42, Ordering::Relaxed);
            let _ = shutdown_tx.send(());
            true
        }
        "/stop" => {
            info!("🛑 收到 /stop 命令, 正常关闭...");
            exit_code.store(0, Ordering::Relaxed);
            let _ = shutdown_tx.send(());
            true
        }
        "/status" => {
            let runtime_snapshot = runtime.snapshot().await;
            let status = wechat.check_status().await;
            let listen_list = wechat.get_listen_list().await;
            let db_status = if db.get().is_some() {
                "可用"
            } else {
                "不可用"
            };
            let contacts = if let Some(d) = db.get() {
                d.get_contacts().await.len()
            } else {
                0
            };
            info!("📊 === 运行时状态 ===");
            info!(
                "📊 RuntimeState: {}{}",
                runtime_snapshot.state,
                runtime_snapshot
                    .reason
                    .as_deref()
                    .map(|reason| format!(" ({reason})"))
                    .unwrap_or_default()
            );
            info!("📊 微信状态: {}", status);
            info!("📊 数据库: {} | 联系人: {} 条", db_status, contacts);
            let input = input_metrics.snapshot();
            info!(
                "📊 输入队列: depth={} last={}ms max={}ms failures={}",
                input.queue_depth,
                input.last_command_ms,
                input.max_command_ms,
                input.total_failures
            );
            info!("📊 监听窗口: {} 个 {:?}", listen_list.len(), listen_list);
            info!("📊 版本: v{}", env!("CARGO_PKG_VERSION"));
            info!("📊 ==================");
            false
        }
        "/refresh" => {
            if let Some(d) = db.get() {
                info!("👥 手动刷新联系人...");
                match d.refresh_contacts().await {
                    Ok(n) => info!("👥 刷新完成: {} 条", n),
                    Err(e) => warn!("⚠️ 刷新失败: {}", e),
                }
            } else {
                info!("⚠️ 数据库不可用");
            }
            false
        }
        "/atmode" => {
            let _ = broadcast_tx.send(WxEvent::Control {
                cmd: "toggle_at_mode".to_string(),
            });
            info!("📢 已发送仅@模式切换指令");
            false
        }
        "/reload" => {
            if let Some(ref path) = config_path {
                match std::fs::read_to_string(path) {
                    Ok(content) => match toml::from_str::<AppConfig>(&content) {
                        Ok(new_config) => {
                            // 1. 更新 at_delay_ms
                            let old_delay = wechat.get_at_delay_ms();
                            let new_delay = new_config.timing.at_delay_ms;
                            if old_delay != new_delay {
                                wechat.set_at_delay_ms(new_delay);
                                info!("⚙️ at_delay_ms: {old_delay} → {new_delay}");
                            }
                            // 2. Diff listen 列表
                            let current_list = wechat.get_listen_list().await;
                            let new_list = new_config.listen.auto;
                            // 新增的
                            let to_add: Vec<_> = new_list
                                .iter()
                                .filter(|n| !current_list.contains(n))
                                .cloned()
                                .collect();
                            // 移除的
                            let to_remove: Vec<_> = current_list
                                .iter()
                                .filter(|n| !new_list.contains(n))
                                .cloned()
                                .collect();
                            if to_add.is_empty() && to_remove.is_empty() {
                                info!("⚙️ 监听列表无变化");
                            } else {
                                for who in &to_remove {
                                    info!("👂 /reload 移除监听: {who}");
                                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                                    if api::enqueue_input_command(
                                        input_tx,
                                        input_metrics,
                                        api::InputCommand::RemoveListen {
                                            who: who.clone(),
                                            reply: reply_tx,
                                        },
                                    )
                                    .await
                                    .is_ok()
                                    {
                                        let _ = reply_rx.await;
                                    }
                                }
                                for who in &to_add {
                                    info!("👂 /reload 添加监听: {who}");
                                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                                    if api::enqueue_input_command(
                                        input_tx,
                                        input_metrics,
                                        api::InputCommand::AddListen {
                                            who: who.clone(),
                                            reply: reply_tx,
                                        },
                                    )
                                    .await
                                    .is_ok()
                                    {
                                        match reply_rx.await {
                                            Ok(Ok(true)) => info!("✅ 监听已添加: {who}"),
                                            _ => warn!("⚠️ 添加监听失败: {who}"),
                                        }
                                    }
                                    // 每个目标间隔 3 秒
                                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                }
                            }
                            info!("⚙️ 配置已重新加载");
                        }
                        Err(e) => warn!("⚠️ 配置解析失败: {e}"),
                    },
                    Err(e) => warn!("⚠️ 读取配置失败: {e}"),
                }
            } else {
                info!("⚠️ 未找到配置文件路径, 无法重载");
            }
            false
        }
        "/sessions" => {
            if let Some(d) = db.get() {
                match d.get_sessions().await {
                    Ok(sessions) => {
                        info!("💬 === 会话列表 ({} 个) ===", sessions.len());
                        for s in &sessions {
                            let unread = if s.unread_count > 0 {
                                format!(" [未读:{}]", s.unread_count)
                            } else {
                                String::new()
                            };
                            info!("💬  {} ({}){}", s.display_name, s.username, unread);
                        }
                        info!("💬 ==================");
                    }
                    Err(e) => warn!("⚠️ 获取会话失败: {}", e),
                }
            } else {
                info!("⚠️ 数据库不可用");
            }
            false
        }
        _ if cmd.starts_with("/send ") => {
            let rest = cmd.strip_prefix("/send ").unwrap().trim();
            if let Some((to, text)) = rest.split_once(' ') {
                let to = to.trim();
                let text = text.trim();
                if to.is_empty() || text.is_empty() {
                    info!("❌ 用法: /send <收件人> <内容>");
                } else {
                    info!("📤 发送消息: [{to}] → {text}");
                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                    let sent_rx = db.get().map(|db| db.subscribe_sent());
                    if api::enqueue_input_command(
                        input_tx,
                        input_metrics,
                        api::InputCommand::SendMessage {
                            to: to.to_string(),
                            text: text.to_string(),
                            at: vec![],
                            skip_verify: true,
                            reply: reply_tx,
                        },
                    )
                    .await
                    .is_ok()
                    {
                        match reply_rx.await {
                            Ok(Ok((true, _, msg))) => {
                                let verified = if let (Some(db), Some(rx)) = (db.get(), sent_rx) {
                                    db.verify_sent(text, rx).await.unwrap_or(false)
                                } else {
                                    wechat.verify_sent_after_send(to, text).await
                                };
                                info!("✅ {msg} | verified={verified}");
                            }
                            Ok(Ok((false, _, msg))) => warn!("⚠️ {msg}"),
                            Ok(Err(e)) => warn!("⚠️ 发送失败: {e}"),
                            Err(_) => warn!("⚠️ actor 响应通道已关闭"),
                        }
                    } else {
                        warn!("⚠️ InputEngine actor 已停止");
                    }
                }
            } else {
                info!("❌ 用法: /send <收件人> <内容>");
            }
            false
        }
        _ if cmd.starts_with("/listen ") => {
            let who = cmd.strip_prefix("/listen ").unwrap().trim();
            if who.is_empty() {
                info!("❌ 用法: /listen <联系人/群名>");
            } else {
                info!("👂 添加监听: {who}");
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if api::enqueue_input_command(
                    input_tx,
                    input_metrics,
                    api::InputCommand::AddListen {
                        who: who.to_string(),
                        reply: reply_tx,
                    },
                )
                .await
                .is_ok()
                {
                    match reply_rx.await {
                        Ok(Ok(true)) => {
                            info!("✅ 监听已添加: {who}");
                            // 持久化: 写入 config.toml
                            if let Some(ref path) = config_path {
                                let mut list = wechat.get_listen_list().await;
                                if !list.contains(&who.to_string()) {
                                    list.push(who.to_string());
                                }
                                save_listen_list(path, &list);
                            }
                        }
                        Ok(Ok(false)) => warn!("⚠️ 添加监听失败: {who}"),
                        Ok(Err(e)) => warn!("⚠️ 添加监听错误: {e}"),
                        Err(_) => warn!("⚠️ actor 响应通道已关闭"),
                    }
                } else {
                    warn!("⚠️ InputEngine actor 已停止");
                }
            }
            false
        }
        _ if cmd.starts_with("/unlisten ") => {
            let who = cmd.strip_prefix("/unlisten ").unwrap().trim();
            if who.is_empty() {
                info!("❌ 用法: /unlisten <联系人/群名>");
            } else {
                info!("👂 移除监听: {who}");
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if api::enqueue_input_command(
                    input_tx,
                    input_metrics,
                    api::InputCommand::RemoveListen {
                        who: who.to_string(),
                        reply: reply_tx,
                    },
                )
                .await
                .is_ok()
                {
                    match reply_rx.await {
                        Ok(true) => {
                            info!("✅ 监听已移除: {who}");
                            // 持久化: 写入 config.toml
                            if let Some(ref path) = config_path {
                                let mut list = wechat.get_listen_list().await;
                                list.retain(|n| n != who);
                                save_listen_list(path, &list);
                            }
                        }
                        Ok(false) => info!("⚠️ 未找到监听: {who}"),
                        Err(_) => warn!("⚠️ actor 响应通道已关闭"),
                    }
                } else {
                    warn!("⚠️ InputEngine actor 已停止");
                }
            }
            false
        }
        "/help" => {
            info!("💡 === 可用命令 ===");
            info!("💡 /restart  — 优雅重启    /stop — 关闭程序");
            info!("💡 /status   — 运行状态    /refresh — 刷新联系人");
            info!("💡 /atmode   — 切换仅@模式  /sessions — 查看会话列表");
            info!("💡 /reload   — 热重载配置    /help — 显示帮助");
            info!("💡 /send <收件人> <内容> — 发送消息");
            info!("💡 /listen <名称>       — 添加监听");
            info!("💡 /unlisten <名称>     — 移除监听");
            info!("💡 快捷键: ↑↓历史 ←→光标 Ctrl+U清行 Ctrl+L清屏");
            info!("💡 ==================");
            false
        }
        _ => {
            info!("❓ 未知命令: {} (/help 查看帮助)", cmd);
            false
        }
    }
}

/// 交互式控制台主循环 (raw mode)
async fn console_loop(
    exit_code: Arc<AtomicI32>,
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
    runtime: Arc<RuntimeManager>,
    input_metrics: Arc<api::InputMetrics>,
    wechat: Arc<wechat::WeChat>,
    db: Arc<std::sync::OnceLock<Arc<db::DbManager>>>,
    broadcast_tx: tokio::sync::broadcast::Sender<WxEvent>,
    input_tx: tokio::sync::mpsc::Sender<api::InputCommand>,
    config_path: Option<PathBuf>,
) {
    let _guard = match enable_raw_mode() {
        Some(g) => g,
        None => {
            debug!("📥 非 TTY, 降级为简单模式");
            console_loop_simple(
                exit_code,
                shutdown_tx,
                runtime,
                input_metrics,
                wechat,
                db,
                broadcast_tx,
                input_tx,
                config_path,
            )
            .await;
            return;
        }
    };

    use tokio::io::AsyncReadExt;
    let mut stdin = tokio::io::stdin();
    let mut line = String::new();
    let mut cursor: usize = 0;
    let mut history: Vec<String> = Vec::new();
    let mut hist_idx: usize = 0;

    redraw_prompt(&line, cursor);

    let mut buf = [0u8; 128];
    loop {
        let n = match stdin.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };

        let bytes = &buf[..n];
        let mut i = 0;
        let mut redraw = false;
        let mut exec = false;

        while i < bytes.len() {
            match bytes[i] {
                b'\r' | b'\n' => {
                    exec = true;
                    break;
                }
                0x7f | 0x08 => {
                    // Backspace
                    if cursor > 0 {
                        let prev = line[..cursor]
                            .char_indices()
                            .last()
                            .map(|(p, _)| p)
                            .unwrap_or(0);
                        line.drain(prev..cursor);
                        cursor = prev;
                        redraw = true;
                    }
                    i += 1;
                }
                0x1b if i + 2 < bytes.len() && bytes[i + 1] == b'[' => match bytes[i + 2] {
                    b'A' => {
                        // ↑ 历史
                        if !history.is_empty() && hist_idx > 0 {
                            hist_idx -= 1;
                            line = history[hist_idx].clone();
                            cursor = line.len();
                            redraw = true;
                        }
                        i += 3;
                    }
                    b'B' => {
                        // ↓ 历史
                        if hist_idx < history.len() {
                            hist_idx += 1;
                            line = if hist_idx < history.len() {
                                history[hist_idx].clone()
                            } else {
                                String::new()
                            };
                            cursor = line.len();
                            redraw = true;
                        }
                        i += 3;
                    }
                    b'C' => {
                        // →
                        if cursor < line.len() {
                            cursor = line[cursor..]
                                .char_indices()
                                .nth(1)
                                .map(|(ci, _)| cursor + ci)
                                .unwrap_or(line.len());
                            redraw = true;
                        }
                        i += 3;
                    }
                    b'D' => {
                        // ←
                        if cursor > 0 {
                            cursor = line[..cursor]
                                .char_indices()
                                .last()
                                .map(|(p, _)| p)
                                .unwrap_or(0);
                            redraw = true;
                        }
                        i += 3;
                    }
                    b'H' => {
                        cursor = 0;
                        redraw = true;
                        i += 3;
                    }
                    b'F' => {
                        cursor = line.len();
                        redraw = true;
                        i += 3;
                    }
                    b'3' if i + 3 < bytes.len() && bytes[i + 3] == b'~' => {
                        // Delete
                        if cursor < line.len() {
                            let next = line[cursor..]
                                .char_indices()
                                .nth(1)
                                .map(|(ci, _)| cursor + ci)
                                .unwrap_or(line.len());
                            line.drain(cursor..next);
                            redraw = true;
                        }
                        i += 4;
                    }
                    _ => {
                        i += 3;
                    }
                },
                0x01 => {
                    cursor = 0;
                    redraw = true;
                    i += 1;
                } // Ctrl+A
                0x05 => {
                    cursor = line.len();
                    redraw = true;
                    i += 1;
                } // Ctrl+E
                0x15 => {
                    line.clear();
                    cursor = 0;
                    redraw = true;
                    i += 1;
                } // Ctrl+U
                0x0c => {
                    // Ctrl+L
                    let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\x1b[2J\x1b[H");
                    redraw = true;
                    i += 1;
                }
                b if b >= 0x20 && b < 0x7f => {
                    // ASCII
                    line.insert(cursor, b as char);
                    cursor += 1;
                    redraw = true;
                    i += 1;
                }
                b if b >= 0x80 => {
                    // UTF-8
                    let clen = if b < 0xE0 {
                        2
                    } else if b < 0xF0 {
                        3
                    } else {
                        4
                    };
                    if i + clen <= bytes.len() {
                        if let Ok(s) = std::str::from_utf8(&bytes[i..i + clen]) {
                            line.insert_str(cursor, s);
                            cursor += s.len();
                            redraw = true;
                        }
                    }
                    i += clen;
                }
                _ => {
                    i += 1;
                }
            }
        }

        if redraw && !exec {
            redraw_prompt(&line, cursor);
        }

        if exec {
            let cmd = line.trim().to_string();
            let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\r\n");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            if !cmd.is_empty() {
                if history.last().map(|h| h != &cmd).unwrap_or(true) {
                    history.push(cmd.clone());
                }
                if handle_command(
                    &cmd,
                    &exit_code,
                    &shutdown_tx,
                    &runtime,
                    &input_metrics,
                    &wechat,
                    &db,
                    &broadcast_tx,
                    &input_tx,
                    &config_path,
                )
                .await
                {
                    return;
                }
            }
            line.clear();
            cursor = 0;
            hist_idx = history.len();
            redraw_prompt(&line, cursor);
        }
    }
}

/// 简单控制台 (非 TTY 降级模式)
async fn console_loop_simple(
    exit_code: Arc<AtomicI32>,
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
    runtime: Arc<RuntimeManager>,
    input_metrics: Arc<api::InputMetrics>,
    wechat: Arc<wechat::WeChat>,
    db: Arc<std::sync::OnceLock<Arc<db::DbManager>>>,
    broadcast_tx: tokio::sync::broadcast::Sender<WxEvent>,
    input_tx: tokio::sync::mpsc::Sender<api::InputCommand>,
    config_path: Option<PathBuf>,
) {
    use tokio::io::AsyncBufReadExt;
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let cmd = line.trim().to_string();
                if !cmd.is_empty() {
                    if handle_command(
                        &cmd,
                        &exit_code,
                        &shutdown_tx,
                        &runtime,
                        &input_metrics,
                        &wechat,
                        &db,
                        &broadcast_tx,
                        &input_tx,
                        &config_path,
                    )
                    .await
                    {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
}
