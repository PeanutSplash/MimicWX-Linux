//! 数据库监听模块
//!
//! 通过 SQLCipher 解密 + fanotify 监听 WAL 文件变化，实现:
//! - 联系人查询 (contact.db)
//! - 会话列表 (session.db)
//! - 增量消息获取 (message_0.db)
//!
//! 替代原有 AT-SPI2 轮询方案，完全非侵入。
//!
//! v0.4.0 优化: fanotify + PID 过滤替代 inotify (消除自循环冷却期),
//!             持久化 message_0.db 连接 (消除每次 PBKDF2 开销).
//!
//! 设计: rusqlite::Connection 是 !Send, 不能跨 .await 持有。
//! 策略: 所有 DB 操作在 spawn_blocking 中完成, 异步方法只操作缓存。

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error, info, trace, warn};

use crate::keyscan::{DbCatalog, KeyRegistry};

// =====================================================================
// 类型定义
// =====================================================================

/// 联系人信息
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContactInfo {
    pub username: String,
    pub nick_name: String,
    pub remark: String,
    pub alias: String,
    /// 优先显示名: remark > nick_name > username
    pub display_name: String,
}

/// 会话信息 (来自数据库)
#[derive(Debug, Clone, serde::Serialize)]
pub struct DbSessionInfo {
    pub username: String,
    pub display_name: String,
    pub unread_count: i32,
    pub summary: String,
    pub last_timestamp: i64,
    pub last_msg_sender: String,
}

/// 结构化消息内容 (按 msg_type 解析)
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", content = "data")]
pub enum MsgContent {
    /// 纯文本 (msg_type=1)
    Text { text: String },
    /// 图片 (msg_type=3)
    Image { path: Option<String> },
    /// 语音 (msg_type=34)
    Voice { duration_ms: Option<u32> },
    /// 视频 (msg_type=43)
    Video { thumb_path: Option<String> },
    /// 表情包 (msg_type=47)
    Emoji { url: Option<String> },
    /// 链接/文件/小程序 (msg_type=49)
    App {
        title: Option<String>,
        desc: Option<String>,
        url: Option<String>,
        app_type: Option<i32>,
    },
    /// 系统消息 (msg_type=10000/10002)
    System { text: String },
    /// 未知类型
    Unknown { raw: String, msg_type: i64 },
}

impl MsgContent {
    /// 消息类型的简短描述 (用于日志)
    #[allow(dead_code)]
    pub fn type_label(&self) -> &'static str {
        match self {
            Self::Text { .. } => "文本",
            Self::Image { .. } => "图片",
            Self::Voice { .. } => "语音",
            Self::Video { .. } => "视频",
            Self::Emoji { .. } => "表情",
            Self::App { .. } => "链接",
            Self::System { .. } => "系统",
            Self::Unknown { .. } => "未知",
        }
    }

    /// 日志预览文本
    pub fn preview(&self, max_len: usize) -> String {
        let text = match self {
            Self::Text { text } => text.clone(),
            Self::Image { .. } => "[图片]".into(),
            Self::Voice { duration_ms, .. } => match duration_ms {
                Some(ms) if *ms >= 1000 => format!("[语音 {}s]", ms / 1000),
                Some(ms) if *ms > 0 => format!("[语音 {ms}ms]"),
                _ => "[语音]".into(),
            },
            Self::Video { .. } => "[视频]".into(),
            Self::Emoji { url, .. } => format!("[表情] {}", url.as_deref().unwrap_or("")),
            Self::App {
                title,
                desc,
                app_type,
                ..
            } => {
                let t = title.as_deref().unwrap_or("");
                let d = desc.as_deref().unwrap_or("");
                // 子类型 + 标题后缀推断
                let label = match app_type.unwrap_or(0) {
                    3 => "音乐",
                    6 => "文件",
                    19 => "转发",
                    33 | 36 => "小程序",
                    42 => "名片",
                    2000 => "转账",
                    2001 => "红包",
                    _ => {
                        // 子类型提取失败时, 用标题后缀推断文件
                        let tl = t.to_lowercase();
                        if tl.ends_with(".pdf")
                            || tl.ends_with(".doc")
                            || tl.ends_with(".docx")
                            || tl.ends_with(".xls")
                            || tl.ends_with(".xlsx")
                            || tl.ends_with(".ppt")
                            || tl.ends_with(".pptx")
                            || tl.ends_with(".zip")
                            || tl.ends_with(".rar")
                            || tl.ends_with(".7z")
                            || tl.ends_with(".txt")
                            || tl.ends_with(".csv")
                            || tl.ends_with(".apk")
                            || tl.ends_with(".exe")
                            || tl.ends_with(".dmg")
                        {
                            "文件"
                        } else {
                            "链接"
                        }
                    }
                };
                if !t.is_empty() {
                    format!("[{label}] {t}")
                } else if !d.is_empty() {
                    format!("[{label}] {d}")
                } else {
                    format!("[{label}]")
                }
            }
            Self::System { text } => format!("[系统] {text}"),
            Self::Unknown { msg_type, .. } => format!("[type={msg_type}]"),
        };
        if text.len() > max_len {
            format!("{}...", &text[..text.floor_char_boundary(max_len)])
        } else {
            text
        }
    }
}

/// 数据库消息
#[derive(Debug, Clone, serde::Serialize)]
pub struct DbMessage {
    pub local_id: i64,
    pub server_id: i64,
    pub create_time: i64,
    /// 原始 content 字符串 (向后兼容)
    pub content: String,
    /// 结构化解析结果
    pub parsed: MsgContent,
    pub msg_type: i64,
    /// 发言人 wxid (群聊中有意义)
    pub talker: String,
    /// 发言人显示名 (通过联系人缓存解析)
    pub talker_display_name: String,
    /// 所属会话
    pub chat: String,
    /// 所属会话显示名
    pub chat_display_name: String,
    /// 是否为自己发送的消息
    pub is_self: bool,
    /// 是否 @ 了自己 (基于 source 列的 atuserlist 精确匹配 wxid)
    pub is_at_me: bool,
    /// 被 @ 的 wxid 列表 (来自 source 列 <atuserlist>)
    pub at_user_list: Vec<String>,
}

#[derive(Debug, Clone)]
struct SessionSnapshot {
    username: String,
    unread_count: i32,
    summary: String,
    last_timestamp: i64,
    last_msg_type: i64,
    last_msg_sender: String,
    last_sender_display_name: String,
}

pub struct DbManager {
    /// 数据库目录快照 (启动时枚举出的数据库列表)
    catalog: Arc<DbCatalog>,
    /// 已验证的数据库密钥注册表 (相对路径 → raw key)
    key_registry: Arc<KeyRegistry>,
    /// 数据库存储目录 (如 /home/wechat/.local/share/weixin/db_storage/)
    db_dir: PathBuf,
    /// 当前登录账号的 wxid (从 db_dir 路径提取, 用于判断自发消息)
    self_wxid: String,
    /// 当前账号的显示名 (从联系人库查询, 默认 "我")
    self_display_name: tokio::sync::RwLock<String>,
    /// 联系人缓存: username → ContactInfo
    contacts: Mutex<HashMap<String, ContactInfo>>,
    /// SessionTable 快照: username → 最新摘要/时间戳
    session_state: Mutex<HashMap<String, SessionSnapshot>>,
    /// 持久化 contact.db 连接 (避免每次重做 PBKDF2)
    contact_conn: Arc<std::sync::Mutex<Option<Connection>>>,
    /// 持久化 session.db 连接
    session_conn: Arc<std::sync::Mutex<Option<Connection>>>,
    /// WAL 变化广播通知 (多消费者: 消息循环 + verify_sent 等)
    wal_notify: tokio::sync::broadcast::Sender<()>,
    /// 自发消息内容广播 (get_new_messages 检测到自发消息时发出)
    sent_content_tx: tokio::sync::broadcast::Sender<String>,
}

impl DbManager {
    /// 创建 DbManager
    pub fn new(catalog: Arc<DbCatalog>, key_registry: Arc<KeyRegistry>) -> Result<Self> {
        let db_dir = catalog.db_dir().to_path_buf();
        debug!("DbManager 初始化: db_dir={}", db_dir.display());

        // 从 db_dir 路径提取自己的 wxid
        // 路径格式: .../wxid_xxx_c024/db_storage
        let self_wxid = db_dir
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .find(|s| s.starts_with("wxid_"))
            .map(|s| {
                // 去掉目录名中的设备后缀 (如 _c024, _ac17 等)
                // wxid 本体一般为 wxid_xxxx 格式, 后缀由微信附加
                if let Some(pos) = s.rfind('_') {
                    let suffix = &s[pos + 1..];
                    // 后缀较短 (≤6字符) 且不以 wxid 开头 → 视为设备后缀
                    if suffix.len() <= 6
                        && suffix.len() >= 2
                        && suffix.chars().all(|c| c.is_ascii_alphanumeric())
                        && !suffix.starts_with("wxid")
                    {
                        return s[..pos].to_string();
                    }
                }
                s.to_string()
            })
            .unwrap_or_default();
        if !self_wxid.is_empty() {
            debug!("当前账号: {}", self_wxid);
        }

        let (wal_tx, _) = tokio::sync::broadcast::channel::<()>(64);
        let (sent_tx, _) = tokio::sync::broadcast::channel::<String>(32);
        Ok(Self {
            catalog,
            key_registry,
            db_dir,
            self_wxid,
            self_display_name: tokio::sync::RwLock::new("我".to_string()),
            contacts: Mutex::new(HashMap::new()),
            session_state: Mutex::new(HashMap::new()),
            contact_conn: Arc::new(std::sync::Mutex::new(None)),
            session_conn: Arc::new(std::sync::Mutex::new(None)),
            wal_notify: wal_tx,
            sent_content_tx: sent_tx,
        })
    }

    /// 启动阶段验证关键数据库是否能被真实打开。
    ///
    /// 成功条件:
    /// - `contact/contact.db` 可打开并通过 SQLCipher 验证
    /// - `session/session.db` 可打开并通过 SQLCipher 验证
    /// - 至少一个 `message_*.db` 可打开并通过 SQLCipher 验证
    pub fn validate_required(&self) -> Result<Vec<String>> {
        let mut validated = Vec::new();

        {
            let mut guard = self
                .contact_conn
                .lock()
                .map_err(|e| anyhow::anyhow!("contact_conn lock: {}", e))?;
            if guard.is_none() {
                *guard = Some(Self::open_db(
                    self.catalog.as_ref(),
                    self.key_registry.as_ref(),
                    &self.db_dir,
                    "contact/contact.db",
                )?);
            }
            validated.push("contact/contact.db".to_string());
        }

        {
            let mut guard = self
                .session_conn
                .lock()
                .map_err(|e| anyhow::anyhow!("session_conn lock: {}", e))?;
            if guard.is_none() {
                *guard = Some(Self::open_db(
                    self.catalog.as_ref(),
                    self.key_registry.as_ref(),
                    &self.db_dir,
                    "session/session.db",
                )?);
            }
            validated.push("session/session.db".to_string());
        }

        let mut candidates: Vec<String> = self.catalog.message_paths().map(str::to_string).collect();
        anyhow::ensure!(!candidates.is_empty(), "未发现 message_*.db");
        candidates.sort();
        if let Some(pos) = candidates.iter().position(|path| path == "message/message_0.db") {
            let preferred = candidates.remove(pos);
            candidates.insert(0, preferred);
        }

        let mut last_err = None;
        for rel_path in candidates {
            match Self::open_db(
                self.catalog.as_ref(),
                self.key_registry.as_ref(),
                &self.db_dir,
                &rel_path,
            ) {
                Ok(_) => {
                    validated.push(rel_path);
                    return Ok(validated);
                }
                Err(err) => {
                    debug!("关键消息库验证失败 {}: {}", rel_path, err);
                    last_err = Some((rel_path, err));
                    continue;
                }
            }
        }

        match last_err {
            Some((rel_path, err)) => Err(err).with_context(|| format!("关键消息库不可用: {rel_path}")),
            None => anyhow::bail!("无可用的 message_*.db"),
        }
    }

    // =================================================================
    // 数据库连接 (同步, 在 spawn_blocking 中调用)
    // =================================================================

    /// 打开加密数据库 (只读模式)
    fn open_db(
        catalog: &DbCatalog,
        key_registry: &KeyRegistry,
        db_dir: &Path,
        db_name: &str,
    ) -> Result<Connection> {
        let path = db_dir.join(db_name);
        anyhow::ensure!(path.exists(), "数据库不存在: {}", path.display());
        let enc_key = key_registry.enc_key_for(db_name)?;
        let salt = *catalog
            .entry(db_name)
            .ok_or_else(|| anyhow::anyhow!("数据库目录快照中不存在: {db_name}"))?
            .salt();
        let pragma_key = format!(
            "PRAGMA key = \"x'{}{}'\";",
            hex_encode(&enc_key),
            hex_encode(&salt)
        );

        // WAL 模式下必须用 READ_WRITE 才能读到 WAL 中未 checkpoint 的新数据
        // 配合 PRAGMA query_only=ON 防止意外写入
        let conn = Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("打开数据库失败: {}", path.display()))?;

        // PR18 扫描命中的是 per-DB enc_key，不是旧 GDB 方案里的口令态 raw key。
        // SQLCipher 需要按 raw-key + salt 形式接入，不能继续走 sqlite3_key(enc_key)。
        conn.execute_batch(&pragma_key)?;
        conn.execute_batch("PRAGMA cipher_compatibility = 4;")?;
        // 安全防护: 不触发 checkpoint, 不写入数据
        conn.execute_batch("PRAGMA wal_autocheckpoint = 0;")?;
        conn.execute_batch("PRAGMA query_only = ON;")?;
        // 防御性: 遇到写锁时等待最多 5 秒, 而非直接报错
        conn.execute_batch("PRAGMA busy_timeout = 5000;")?;

        // 验证解密成功
        let count: i32 = conn
            .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
            .with_context(|| format!("数据库解密验证失败: {}", db_name))?;

        trace!("🔓 {} 解密成功, {} 个表", db_name, count);
        Ok(conn)
    }

    // =================================================================
    // 联系人
    // =================================================================

    /// 加载/刷新联系人缓存 (spawn_blocking 中执行 DB 查询)
    pub async fn refresh_contacts(&self) -> Result<usize> {
        let catalog = Arc::clone(&self.catalog);
        let registry = Arc::clone(&self.key_registry);
        let dir = self.db_dir.clone();
        let conn_mutex = Arc::clone(&self.contact_conn);

        let contacts = tokio::task::spawn_blocking(move || -> Result<Vec<ContactInfo>> {
            // 复用或创建持久连接
            let mut guard = conn_mutex
                .lock()
                .map_err(|e| anyhow::anyhow!("contact_conn lock: {}", e))?;
            if guard.is_none() {
                *guard = Some(Self::open_db(
                    catalog.as_ref(),
                    registry.as_ref(),
                    &dir,
                    "contact/contact.db",
                )?);
                debug!("contact.db 持久连接已建立");
            }
            let conn = guard.as_ref().unwrap();
            let mut stmt =
                conn.prepare("SELECT username, nick_name, remark, alias FROM contact")?;
            // WCDB 压缩可能导致 TEXT 列实际存储为 BLOB (Zstd),
            // 必须用 BLOB 回退读取, 否则部分行 (包括 chatroom) 会被丢弃
            let result: Vec<ContactInfo> = stmt
                .query_map([], |row| {
                    let username = wcdb_get_text(row, 0);
                    if username.is_empty() {
                        return Err(rusqlite::Error::InvalidQuery);
                    }
                    let nick_name = wcdb_get_text(row, 1);
                    let remark = wcdb_get_text(row, 2);
                    let alias = wcdb_get_text(row, 3);
                    let display_name = if !remark.is_empty() {
                        remark.clone()
                    } else if !nick_name.is_empty() {
                        nick_name.clone()
                    } else {
                        username.clone()
                    };
                    Ok(ContactInfo {
                        username,
                        nick_name,
                        remark,
                        alias,
                        display_name,
                    })
                })?
                .filter_map(|r| match r {
                    Ok(c) => Some(c),
                    Err(e) => {
                        warn!("⚠️ 联系人行读取失败: {}", e);
                        None
                    }
                })
                .collect();
            Ok(result)
        })
        .await??;

        let count = contacts.len();
        // 短暂持锁: 清空并填入联系人
        {
            let mut cache = self.contacts.lock().await;
            cache.clear();
            for c in contacts {
                cache.insert(c.username.clone(), c);
            }
        } // 锁在此释放, 不阻塞 get_new_messages 等热路径
        debug!("联系人缓存: {} 条", count);

        // 从 chat_room 表补充群名 (锁已释放, spawn_blocking 不会阻塞读操作)
        let chatrooms = {
            let conn_mutex2 = Arc::clone(&self.contact_conn);
            tokio::task::spawn_blocking(move || -> Result<Vec<(String, String)>> {
                let guard = conn_mutex2
                    .lock()
                    .map_err(|e| anyhow::anyhow!("contact_conn lock: {}", e))?;
                if let Some(conn) = guard.as_ref() {
                    let mut result = Vec::new();
                    if let Ok(mut stmt) = conn.prepare(
                        "SELECT cr.username, c.nick_name FROM chat_room cr \
                         LEFT JOIN contact c ON cr.username = c.username \
                         WHERE cr.username IS NOT NULL",
                    ) {
                        let rows: Vec<(String, String)> = stmt
                            .query_map([], |row| {
                                let id = wcdb_get_text(row, 0);
                                let name = wcdb_get_text(row, 1);
                                Ok((id, name))
                            })
                            .ok()
                            .map(|iter| iter.filter_map(|r| r.ok()).collect())
                            .unwrap_or_default();

                        for (id, name) in rows {
                            if !id.is_empty() && !name.is_empty() {
                                debug!("👥 chat_room 补充: {} → {}", id, name);
                                result.push((id, name));
                            }
                        }
                    }
                    Ok(result)
                } else {
                    Ok(vec![])
                }
            })
            .await
            .unwrap_or_else(|_| Ok(vec![]))
            .unwrap_or_default()
        };

        // 短暂持锁: 补充群名
        if !chatrooms.is_empty() {
            let mut cache = self.contacts.lock().await;
            let mut added = 0usize;
            for (chatroom_id, nick_name) in chatrooms {
                if !cache.contains_key(&chatroom_id) {
                    cache.insert(
                        chatroom_id.clone(),
                        ContactInfo {
                            username: chatroom_id,
                            nick_name: nick_name.clone(),
                            remark: String::new(),
                            alias: String::new(),
                            display_name: nick_name,
                        },
                    );
                    added += 1;
                }
            }
            if added > 0 {
                debug!("群聊名称补充: {} 条", added);
            }
        }

        // 尝试解析当前账号的显示名 (短暂持锁读取, 然后释放)
        if !self.self_wxid.is_empty() {
            let name = self
                .contacts
                .lock()
                .await
                .get(&self.self_wxid)
                .map(|c| c.display_name.clone());
            if let Some(name) = name {
                debug!("当前账号昵称: {} ({})", name, self.self_wxid);
                *self.self_display_name.write().await = name;
            }
        }

        Ok(count)
    }

    /// 获取联系人列表
    pub async fn get_contacts(&self) -> Vec<ContactInfo> {
        self.contacts.lock().await.values().cloned().collect()
    }

    /// 通过 username 获取显示名
    async fn resolve_name(&self, username: &str) -> String {
        self.contacts
            .lock()
            .await
            .get(username)
            .map(|c| c.display_name.clone())
            .unwrap_or_else(|| username.to_string())
    }

    // =================================================================
    // 会话
    // =================================================================

    /// 获取会话列表
    pub async fn get_sessions(&self) -> Result<Vec<DbSessionInfo>> {
        let catalog = Arc::clone(&self.catalog);
        let registry = Arc::clone(&self.key_registry);
        let dir = self.db_dir.clone();
        let conn_mutex = Arc::clone(&self.session_conn);

        let rows = tokio::task::spawn_blocking(
            move || -> Result<Vec<(String, i32, String, i64, String)>> {
                // 复用或创建持久连接
                let mut guard = conn_mutex
                    .lock()
                    .map_err(|e| anyhow::anyhow!("session_conn lock: {}", e))?;
                if guard.is_none() {
                    *guard = Some(Self::open_db(
                        catalog.as_ref(),
                        registry.as_ref(),
                        &dir,
                        "session/session.db",
                    )?);
                    debug!("session.db 持久连接已建立");
                }
                let conn = guard.as_ref().unwrap();
                let mut stmt = conn.prepare(
                    "SELECT username, unread_count, summary, last_timestamp, last_msg_sender \
                 FROM SessionTable ORDER BY sort_timestamp DESC",
                )?;
                let result = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<i32>>(1)?.unwrap_or(0),
                            row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                            row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                            row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        ))
                    })?
                    .filter_map(|r| r.ok())
                    .collect();
                Ok(result)
            },
        )
        .await??;

        // 异步填充显示名
        let mut sessions = Vec::with_capacity(rows.len());
        for (username, unread_count, summary, last_timestamp, last_msg_sender) in rows {
            let display_name = self.resolve_name(&username).await;
            sessions.push(DbSessionInfo {
                username,
                display_name,
                unread_count,
                summary,
                last_timestamp,
                last_msg_sender,
            });
        }
        Ok(sessions)
    }

    // =================================================================
    // 增量消息
    // =================================================================

    async fn load_session_snapshots(&self) -> Result<Vec<SessionSnapshot>> {
        let catalog = Arc::clone(&self.catalog);
        let registry = Arc::clone(&self.key_registry);
        let dir = self.db_dir.clone();
        let conn_mutex = Arc::clone(&self.session_conn);

        tokio::task::spawn_blocking(move || -> Result<Vec<SessionSnapshot>> {
            let mut guard = conn_mutex
                .lock()
                .map_err(|e| anyhow::anyhow!("session_conn lock: {}", e))?;
            if guard.is_none() {
                *guard = Some(Self::open_db(
                    catalog.as_ref(),
                    registry.as_ref(),
                    &dir,
                    "session/session.db",
                )?);
                debug!("session.db 持久连接已建立");
            }
            let conn = guard.as_ref().unwrap();
            let mut stmt = conn.prepare(
                "SELECT username, unread_count, summary, last_timestamp, \
                        last_msg_type, last_msg_sender, last_sender_display_name \
                 FROM SessionTable \
                 WHERE last_timestamp > 0",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(SessionSnapshot {
                        username: wcdb_get_text(row, 0),
                        unread_count: row.get::<_, Option<i32>>(1)?.unwrap_or(0),
                        summary: wcdb_get_text(row, 2),
                        last_timestamp: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                        last_msg_type: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                        last_msg_sender: wcdb_get_text(row, 5),
                        last_sender_display_name: wcdb_get_text(row, 6),
                    })
                })?
                .filter_map(|row| match row {
                    Ok(snapshot) if !snapshot.username.is_empty() => Some(snapshot),
                    Ok(_) => None,
                    Err(err) => {
                        warn!("⚠️ SessionTable 行读取失败: {}", err);
                        None
                    }
                })
                .collect();
            Ok(rows)
        })
        .await?
    }

    pub async fn prime_session_state(&self) -> Result<usize> {
        let snapshots = self.load_session_snapshots().await?;
        let count = snapshots.len();
        let mut state = self.session_state.lock().await;
        state.clear();
        for snapshot in snapshots {
            state.insert(snapshot.username.clone(), snapshot);
        }
        Ok(count)
    }

    fn is_session_update(prev: Option<&SessionSnapshot>, curr: &SessionSnapshot) -> bool {
        match prev {
            None => false,
            Some(prev) => {
                curr.last_timestamp > prev.last_timestamp
                    || (curr.last_timestamp == prev.last_timestamp
                        && (curr.last_msg_type != prev.last_msg_type
                            || curr.summary != prev.summary))
            }
        }
    }

    async fn build_message_from_session(&self, snapshot: &SessionSnapshot) -> DbMessage {
        let chat_display = self.resolve_name(&snapshot.username).await;
        let summary = strip_session_summary(&snapshot.summary);
        let base_msg_type = (snapshot.last_msg_type & 0xFFFF) as i32;

        let inferred_self = if snapshot.username.contains("@chatroom") {
            !self.self_wxid.is_empty() && snapshot.last_msg_sender == self.self_wxid
        } else {
            (!self.self_wxid.is_empty() && snapshot.last_msg_sender == self.self_wxid)
                || (snapshot.last_msg_sender.is_empty() && snapshot.unread_count == 0)
        };

        let talker = if snapshot.username.contains("@chatroom") {
            if inferred_self {
                self.self_wxid.clone()
            } else if !snapshot.last_msg_sender.is_empty() {
                snapshot.last_msg_sender.clone()
            } else {
                snapshot.username.clone()
            }
        } else if inferred_self {
            self.self_wxid.clone()
        } else if !snapshot.last_msg_sender.is_empty() {
            snapshot.last_msg_sender.clone()
        } else {
            snapshot.username.clone()
        };

        let talker_display_name = if inferred_self {
            self.self_display_name.read().await.clone()
        } else if snapshot.username.contains("@chatroom") && !snapshot.last_sender_display_name.is_empty()
        {
            snapshot.last_sender_display_name.clone()
        } else {
            self.resolve_name(&talker).await
        };

        let parsed = parse_msg_content(base_msg_type as i64, &summary);

        DbMessage {
            local_id: 0,
            server_id: 0,
            create_time: snapshot.last_timestamp,
            content: summary,
            parsed,
            msg_type: base_msg_type as i64,
            talker,
            talker_display_name,
            chat: snapshot.username.clone(),
            chat_display_name: chat_display,
            is_self: inferred_self,
            is_at_me: false,
            at_user_list: Vec::new(),
        }
    }

    /// 获取新消息 (基于 session.db 的 SessionTable 变化)
    pub async fn get_new_messages(&self) -> Result<Vec<DbMessage>> {
        let snapshots = self.load_session_snapshots().await?;
        let previous = self.session_state.lock().await.clone();
        let mut current = HashMap::with_capacity(snapshots.len());
        let mut updates = Vec::new();

        for snapshot in snapshots {
            let prev = previous.get(&snapshot.username);
            if Self::is_session_update(prev, &snapshot) {
                let msg = self.build_message_from_session(&snapshot).await;
                updates.push(msg);
            }
            current.insert(snapshot.username.clone(), snapshot);
        }

        *self.session_state.lock().await = current;

        for msg in &updates {
            if msg.is_self && !msg.content.is_empty() {
                let _ = self.sent_content_tx.send(msg.content.clone());
            }

            let preview = msg.parsed.preview(40);
            let icon = if msg.is_self { "📤 →" } else { "📨" };
            if msg.chat.contains("@chatroom") {
                info!(
                    "{icon} [{}] {}({}): {}",
                    msg.chat_display_name, msg.talker_display_name, msg.talker, preview
                );
            } else {
                info!("{icon} {}({}): {}", msg.chat_display_name, msg.talker, preview);
            }
        }

        Ok(updates)
    }

    // =================================================================
    // 发送验证 (DB 版)
    // =================================================================

    /// 通过数据库验证消息是否发送成功 (事件驱动)
    ///
    /// 订阅 get_new_messages 的自发消息广播, 等待内容匹配.
    /// 无需单独查询 DB, 完全复用现有的消息检测流程.
    /// 调用方应在发送前调用 subscribe_sent() 获取 receiver, 避免竞态.
    /// 超时 5 秒兜底.
    pub async fn verify_sent(
        &self,
        text: &str,
        mut sent_rx: tokio::sync::broadcast::Receiver<String>,
    ) -> Result<bool> {
        let text_owned = text.to_string();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            tokio::select! {
                result = sent_rx.recv() => {
                    match result {
                        Ok(content) => {
                            let content_trimmed = content.trim();
                            if !content_trimmed.is_empty() && (
                                content_trimmed.contains(&text_owned)
                                || text_owned.contains(content_trimmed)
                            ) {
                                debug!("[DB] 发送验证成功");
                                return Ok(true);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            debug!("[DB] 自发消息广播通道已关闭");
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    debug!("[DB] 发送验证超时 (5s)");
                    break;
                }
            }
        }
        Ok(false)
    }

    /// 订阅自发消息广播 (在发送前调用, 确保不丢失发送期间的事件)
    pub fn subscribe_sent(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.sent_content_tx.subscribe()
    }

    /// 订阅 session.db 变化通知
    #[allow(dead_code)]
    pub fn subscribe_wal_events(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.wal_notify.subscribe()
    }

    // =================================================================
    // session.db mtime 监听
    // =================================================================

    /// 启动 session.db 变化监听 (mtime 轮询, 在独立线程运行)
    ///
    /// 返回 broadcast::Receiver, 支持多消费者 (消息循环 + verify_sent 等)
    pub fn spawn_wal_watcher(self: &Arc<Self>) -> tokio::sync::broadcast::Receiver<()> {
        let wal_tx = self.wal_notify.clone();
        let db_dir = self.db_dir.clone();

        std::thread::spawn(move || {
            if let Err(e) = wal_watch_loop(&db_dir, wal_tx) {
                error!("❌ Session 监听退出: {}", e);
            }
        });

        info!("👁️ Session DB 监听已启动 (mtime 轮询, 30ms)");
        self.wal_notify.subscribe()
    }
}

// =====================================================================
// 同步辅助函数
// =====================================================================

// =====================================================================
// session.db 监听 (mtime 轮询, 在 std::thread 中运行)
// =====================================================================

fn wal_watch_loop(db_dir: &Path, tx: tokio::sync::broadcast::Sender<()>) -> Result<()> {
    let session_dir = db_dir.join("session");
    let session_db = session_dir.join("session.db");
    let session_wal = session_dir.join("session.db-wal");
    let poll_interval = std::time::Duration::from_millis(30);

    while !session_db.exists() {
        debug!("等待 session.db 创建: {}", session_db.display());
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    debug!(
        "开始监听 session mtime: db={} wal={}",
        session_db.display(),
        session_wal.display()
    );

    let mut prev_db = file_mtime(&session_db);
    let mut prev_wal = file_mtime(&session_wal);

    loop {
        std::thread::sleep(poll_interval);

        let curr_db = file_mtime(&session_db);
        let curr_wal = file_mtime(&session_wal);
        if curr_db == prev_db && curr_wal == prev_wal {
            continue;
        }

        trace!(
            "📝 session mtime 变化: db={:?}->{:?} wal={:?}->{:?}",
            prev_db,
            curr_db,
            prev_wal,
            curr_wal
        );
        prev_db = curr_db;
        prev_wal = curr_wal;
        let _ = tx.send(());
    }
}

// =====================================================================
// 消息内容解析
// =====================================================================

/// WCDB Zstd BLOB 解压: 检测 Zstd magic 0x28B52FFD, 解压后返回 UTF-8 字符串
fn decompress_wcdb_content(blob: &[u8]) -> String {
    // Zstd magic: 0xFD2FB528 (little-endian) = bytes [0x28, 0xB5, 0x2F, 0xFD]
    if blob.len() >= 4 && blob[0] == 0x28 && blob[1] == 0xB5 && blob[2] == 0x2F && blob[3] == 0xFD {
        match zstd::decode_all(blob) {
            Ok(data) => return String::from_utf8_lossy(&data).to_string(),
            Err(e) => warn!("⚠️ Zstd 解压失败: {}", e),
        }
    }
    // 非 Zstd: 直接 lossy UTF-8
    String::from_utf8_lossy(blob).to_string()
}

fn strip_session_summary(summary: &str) -> String {
    let summary = summary.trim();
    if let Some((_, content)) = summary.split_once(":\n") {
        return content.trim().to_string();
    }
    if let Some((_, content)) = summary.split_once(":\r\n") {
        return content.trim().to_string();
    }
    summary.to_string()
}

fn file_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// WCDB 兼容读取: 先尝试 TEXT, 失败则 BLOB + Zstd 解压
/// (WCDB 压缩可能导致 TEXT 列实际存储为 BLOB)
fn wcdb_get_text(row: &rusqlite::Row, idx: usize) -> String {
    match row.get::<_, Option<String>>(idx) {
        Ok(s) => s.unwrap_or_default(),
        Err(_) => match row.get::<_, Option<Vec<u8>>>(idx) {
            Ok(Some(bytes)) => decompress_wcdb_content(&bytes),
            _ => String::new(),
        },
    }
}

/// 根据 msg_type 解析原始 content 为结构化 MsgContent
/// content 已经过 Zstd 解压 (如果需要), 应为 XML 或纯文本
fn parse_msg_content(msg_type: i64, content: &str) -> MsgContent {
    // 微信 msg_type 高位是标志位 (如 0x600000021), 实际类型在低 16 位
    let base_type = (msg_type & 0xFFFF) as i32;
    match base_type {
        1 => MsgContent::Text {
            text: content.to_string(),
        },
        3 => parse_image(content),
        34 => parse_voice(content),
        42 => parse_contact_card(content),
        43 => parse_video(content),
        47 => parse_emoji(content),
        49 => parse_app(content),
        10000 | 10002 => MsgContent::System {
            text: content.to_string(),
        },
        _ => MsgContent::Unknown {
            raw: content.to_string(),
            msg_type,
        },
    }
}

/// 图片消息: 从 XML 中提取 CDN URL
fn parse_image(content: &str) -> MsgContent {
    let path = extract_xml_attr(content, "img", "cdnmidimgurl")
        .or_else(|| extract_xml_attr(content, "img", "cdnbigimgurl"));
    MsgContent::Image { path }
}

/// 语音消息: 尝试多种属性名提取时长
fn parse_voice(content: &str) -> MsgContent {
    let duration_ms = extract_xml_attr(content, "voicemsg", "voicelength")
        .or_else(|| extract_xml_attr(content, "voicemsg", "voicelen"))
        .or_else(|| extract_xml_attr(content, "voicemsg", "length"))
        .and_then(|v| v.parse::<u32>().ok());
    MsgContent::Voice { duration_ms }
}

/// 名片消息 (msg_type=42): 提取昵称和 wxid
fn parse_contact_card(content: &str) -> MsgContent {
    let nickname = extract_xml_attr(content, "msg", "nickname")
        .or_else(|| extract_xml_attr(content, "msg", "smallheadimgurl"));
    let username = extract_xml_attr(content, "msg", "username");
    let title = nickname.or(username);
    MsgContent::App {
        title,
        desc: Some("名片".to_string()),
        url: None,
        app_type: Some(42),
    }
}

/// 视频消息: 提取 cdnthumburl
fn parse_video(content: &str) -> MsgContent {
    let thumb_path = extract_xml_attr(content, "videomsg", "cdnthumburl");
    MsgContent::Video { thumb_path }
}

/// 表情消息: 提取 cdnurl
fn parse_emoji(content: &str) -> MsgContent {
    let url = extract_xml_attr(content, "emoji", "cdnurl");
    MsgContent::Emoji { url }
}

/// 链接/文件/小程序消息 (msg_type=49): 解析 appmsg XML
/// app_type 子类型: 3=音乐, 4=链接, 5=链接, 6=文件, 19=转发, 33/36=小程序, 2000=转账, 2001=红包
fn parse_app(content: &str) -> MsgContent {
    let title = extract_xml_text(content, "title");
    let desc = extract_xml_text(content, "des");
    let url = extract_xml_text(content, "url");
    let app_type = extract_xml_text(content, "type").and_then(|t| t.parse::<i32>().ok());
    MsgContent::App {
        title,
        desc,
        url,
        app_type,
    }
}

/// 从 XML 中提取指定元素的属性值 (如 <img cdnmidimgurl="..."/>)
fn extract_xml_attr(xml: &str, tag: &str, attr: &str) -> Option<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                if e.name().as_ref() == tag.as_bytes() {
                    for a in e.attributes().flatten() {
                        if a.key.as_ref() == attr.as_bytes() {
                            return String::from_utf8(a.value.to_vec()).ok();
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

/// 从 XML 中提取指定元素的文本内容 (如 <title>标题</title>)
fn extract_xml_text(xml: &str, tag: &str) -> Option<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut in_tag = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                if e.name().as_ref() == tag.as_bytes() {
                    in_tag = true;
                }
            }
            Ok(Event::Text(ref e)) if in_tag => {
                return e.unescape().ok().map(|s| s.to_string());
            }
            Ok(Event::CData(ref e)) if in_tag => {
                return String::from_utf8(e.to_vec()).ok();
            }
            Ok(Event::End(ref e)) => {
                if e.name().as_ref() == tag.as_bytes() {
                    in_tag = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

// =====================================================================
// 工具函数
// =====================================================================

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

