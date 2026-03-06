//! X11 XTEST 输入引擎
//!
//! 通过 x11rb 使用 X11 XTEST 扩展注入键盘和鼠标事件。
//! 中文输入通过 X11 Selection（剪贴板）+ Ctrl+V 实现。图片通过 xclip + Ctrl+V。

use anyhow::{Context, Result};
use tracing::{debug, info};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    self, AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask, Keycode,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

/// X11 事件类型
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;

/// 延迟常量 (ms)
const KEY_HOLD_MS: u64 = 30;
const TYPING_DELAY_MS: u64 = 20;
const CLICK_HOLD_MS: u64 = 50;

/// X11 Keysym 常量
mod keysym {
    pub const XK_SPACE: u32 = 0x0020;
    pub const XK_RETURN: u32 = 0xFF0D;
    pub const XK_ESCAPE: u32 = 0xFF1B;
    pub const XK_TAB: u32 = 0xFF09;
    pub const XK_BACKSPACE: u32 = 0xFF08;
    pub const XK_DELETE: u32 = 0xFFFF;
    pub const XK_HOME: u32 = 0xFF50;
    pub const XK_END: u32 = 0xFF57;
    pub const XK_LEFT: u32 = 0xFF51;
    pub const XK_UP: u32 = 0xFF52;
    pub const XK_RIGHT: u32 = 0xFF53;
    pub const XK_DOWN: u32 = 0xFF54;
    pub const XK_SHIFT_L: u32 = 0xFFE1;
    pub const XK_CONTROL_L: u32 = 0xFFE3;
    pub const XK_ALT_L: u32 = 0xFFE4;
    pub const XK_F1: u32 = 0xFFBE;
    pub const XK_F2: u32 = 0xFFBF;
    pub const XK_F3: u32 = 0xFFC0;
    pub const XK_F4: u32 = 0xFFC1;
    pub const XK_F5: u32 = 0xFFC2;
}

/// X11 XTEST 输入引擎
pub struct InputEngine {
    conn: RustConnection,
    screen_root: u32,
    min_keycode: Keycode,
    max_keycode: Keycode,
    keysyms_per_keycode: u8,
    keysyms: Vec<u32>,
    // 缓存的 X11 Atom (在 X11 Session 内永不变, 启动时一次性 intern)
    atom_net_wm_name: u32,
    atom_utf8_string: u32,
    atom_net_client_list: u32,
    atom_net_active_window: u32,
    atom_net_close_window: u32,
}

impl InputEngine {
    /// 创建输入引擎
    pub fn new() -> Result<Self> {
        info!("🎮 初始化 X11 XTEST 输入引擎...");

        let display_env = std::env::var("DISPLAY").unwrap_or_else(|_| ":1".into());
        let (conn, screen_num) = RustConnection::connect(Some(&display_env))
            .context(format!("连接 X11 失败 (DISPLAY={display_env})"))?;

        let screen = &conn.setup().roots[screen_num];
        let screen_root = screen.root;

        // 验证 XTEST 扩展
        x11rb::protocol::xtest::get_version(&conn, 2, 2)
            .context("XTEST 扩展不可用")?
            .reply()
            .context("XTEST 版本查询失败")?;

        // 获取键盘映射
        let setup = conn.setup();
        let min_keycode = setup.min_keycode;
        let max_keycode = setup.max_keycode;
        let reply = conn
            .get_keyboard_mapping(min_keycode, max_keycode - min_keycode + 1)?
            .reply()
            .context("获取键盘映射失败")?;

        let keysyms_per_keycode = reply.keysyms_per_keycode;
        let keysyms: Vec<u32> = reply.keysyms.iter().map(|k| (*k).into()).collect();

        // 一次性 intern 所有需要的 Atom (避免每次调用重复查询)
        let atom_net_wm_name = conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
        let atom_utf8_string = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        let atom_net_client_list = conn.intern_atom(false, b"_NET_CLIENT_LIST")?.reply()?.atom;
        let atom_net_active_window = conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
            .reply()?
            .atom;
        let atom_net_close_window = conn.intern_atom(false, b"_NET_CLOSE_WINDOW")?.reply()?.atom;

        info!("✅ X11 XTEST 就绪 (DISPLAY={display_env}, keycodes={min_keycode}~{max_keycode})");

        Ok(Self {
            conn,
            screen_root,
            min_keycode,
            max_keycode,
            keysyms_per_keycode,
            keysyms,
            atom_net_wm_name,
            atom_utf8_string,
            atom_net_client_list,
            atom_net_active_window,
            atom_net_close_window,
        })
    }

    // =================================================================
    // Keysym 查找
    // =================================================================

    fn keysym_to_keycode(&self, keysym: u32) -> Option<(Keycode, bool)> {
        let per = self.keysyms_per_keycode as usize;
        let total = (self.max_keycode - self.min_keycode + 1) as usize;

        for i in 0..total {
            for j in 0..per {
                if self.keysyms[i * per + j] == keysym {
                    let keycode = self.min_keycode + i as u8;
                    let need_shift = j == 1;
                    return Some((keycode, need_shift));
                }
            }
        }
        None
    }

    fn char_to_keysym(ch: char) -> Option<u32> {
        match ch {
            ' ' => Some(keysym::XK_SPACE),
            '\n' => Some(keysym::XK_RETURN),
            '\t' => Some(keysym::XK_TAB),
            c if c.is_ascii() => Some(c as u32),
            _ => None,
        }
    }

    fn key_name_to_keysym(name: &str) -> Option<u32> {
        match name.to_lowercase().as_str() {
            "return" | "enter" => Some(keysym::XK_RETURN),
            "escape" | "esc" => Some(keysym::XK_ESCAPE),
            "tab" => Some(keysym::XK_TAB),
            "backspace" => Some(keysym::XK_BACKSPACE),
            "delete" => Some(keysym::XK_DELETE),
            "space" => Some(keysym::XK_SPACE),
            "home" => Some(keysym::XK_HOME),
            "end" => Some(keysym::XK_END),
            "left" => Some(keysym::XK_LEFT),
            "right" => Some(keysym::XK_RIGHT),
            "up" => Some(keysym::XK_UP),
            "down" => Some(keysym::XK_DOWN),
            "shift" => Some(keysym::XK_SHIFT_L),
            "ctrl" | "control" => Some(keysym::XK_CONTROL_L),
            "alt" => Some(keysym::XK_ALT_L),
            "f1" => Some(keysym::XK_F1),
            "f2" => Some(keysym::XK_F2),
            "f3" => Some(keysym::XK_F3),
            "f4" => Some(keysym::XK_F4),
            "f5" => Some(keysym::XK_F5),
            s if s.len() == 1 => Self::char_to_keysym(s.chars().next()?),
            _ => None,
        }
    }

    // =================================================================
    // 底层 XTEST 操作
    // =================================================================

    fn raw_key_press(&self, keycode: Keycode) -> Result<()> {
        self.conn
            .xtest_fake_input(KEY_PRESS, keycode, 0, self.screen_root, 0, 0, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    fn raw_key_release(&self, keycode: Keycode) -> Result<()> {
        self.conn
            .xtest_fake_input(KEY_RELEASE, keycode, 0, self.screen_root, 0, 0, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    // =================================================================
    // 键盘操作
    // =================================================================

    /// 模拟单次按键
    pub async fn press_key(&mut self, key_name: &str) -> Result<()> {
        let ks = Self::key_name_to_keysym(key_name)
            .ok_or_else(|| anyhow::anyhow!("未知按键: {key_name}"))?;
        let (keycode, need_shift) = self
            .keysym_to_keycode(ks)
            .ok_or_else(|| anyhow::anyhow!("按键无映射: {key_name}"))?;

        // Shift
        let shift_kc = if need_shift {
            self.keysym_to_keycode(keysym::XK_SHIFT_L).map(|(kc, _)| kc)
        } else {
            None
        };
        if let Some(skc) = shift_kc {
            self.raw_key_press(skc)?;
        }

        self.raw_key_press(keycode)?;
        tokio::time::sleep(std::time::Duration::from_millis(KEY_HOLD_MS)).await;
        self.raw_key_release(keycode)?;

        if let Some(skc) = shift_kc {
            self.raw_key_release(skc)?;
        }

        debug!("⌨️ press_key: {key_name}");
        Ok(())
    }

    /// 组合键 (如 "ctrl+f", "ctrl+v", "ctrl+a")
    pub async fn key_combo(&mut self, combo: &str) -> Result<()> {
        let parts: Vec<&str> = combo.split('+').collect();
        let mut keycodes = Vec::new();

        for part in &parts {
            let ks = Self::key_name_to_keysym(part.trim())
                .ok_or_else(|| anyhow::anyhow!("未知按键: {part}"))?;
            let (kc, _) = self
                .keysym_to_keycode(ks)
                .ok_or_else(|| anyhow::anyhow!("按键无映射: {part}"))?;
            keycodes.push(kc);
        }

        // 按顺序按下
        for &kc in &keycodes {
            self.raw_key_press(kc)?;
            tokio::time::sleep(std::time::Duration::from_millis(KEY_HOLD_MS)).await;
        }
        // 逆序释放
        for &kc in keycodes.iter().rev() {
            self.raw_key_release(kc)?;
        }

        debug!("⌨️ key_combo: {combo}");
        Ok(())
    }

    /// 逐字输入 ASCII 文本 (中文请用 paste_text)
    pub async fn type_text(&mut self, text: &str) -> Result<()> {
        for ch in text.chars() {
            let ks = Self::char_to_keysym(ch)
                .ok_or_else(|| anyhow::anyhow!("字符无映射: '{ch}' — 请用 paste_text"))?;
            let (keycode, need_shift) = self
                .keysym_to_keycode(ks)
                .ok_or_else(|| anyhow::anyhow!("字符无 keycode: '{ch}'"))?;

            let shift_kc = if need_shift {
                self.keysym_to_keycode(keysym::XK_SHIFT_L).map(|(kc, _)| kc)
            } else {
                None
            };
            if let Some(skc) = shift_kc {
                self.raw_key_press(skc)?;
            }

            self.raw_key_press(keycode)?;
            tokio::time::sleep(std::time::Duration::from_millis(KEY_HOLD_MS)).await;
            self.raw_key_release(keycode)?;

            if let Some(skc) = shift_kc {
                self.raw_key_release(skc)?;
            }
            tokio::time::sleep(std::time::Duration::from_millis(TYPING_DELAY_MS)).await;
        }
        Ok(())
    }

    /// 通过剪贴板粘贴文本 (支持中文、空格等任意字符)
    pub async fn paste_text(&mut self, text: &str) -> Result<()> {
        self.clipboard_paste(text).await
    }

    async fn clipboard_paste(&mut self, text: &str) -> Result<()> {
        info!("📋 粘贴文本: {} 字符", text.len());
        let display_env = std::env::var("DISPLAY").unwrap_or_else(|_| ":1".into());
        let mut last_error = None;

        for attempt in 0..3 {
            let text_owned = text.to_string();
            let display_env = display_env.clone();
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<()>>();

            let handle = tokio::task::spawn_blocking(move || -> Result<()> {
                use x11rb::connection::Connection;
                use x11rb::protocol::xproto::*;
                use x11rb::protocol::Event;
                use x11rb::wrapper::ConnectionExt as _;

                let (conn, screen_num) =
                    x11rb::rust_connection::RustConnection::connect(Some(&display_env))
                        .context("X11 clipboard 连接失败")?;
                let screen = &conn.setup().roots[screen_num];

                let clipboard = conn.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
                let utf8_string = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
                let targets_atom = conn.intern_atom(false, b"TARGETS")?.reply()?.atom;

                let win = conn.generate_id()?;
                conn.create_window(
                    0,
                    win,
                    screen.root,
                    0,
                    0,
                    1,
                    1,
                    0,
                    WindowClass::INPUT_ONLY,
                    0,
                    &CreateWindowAux::new(),
                )?;
                conn.set_selection_owner(win, clipboard, x11rb::CURRENT_TIME)?;
                conn.flush()?;

                let owner = conn.get_selection_owner(clipboard)?.reply()?.owner;
                if owner != win {
                    let _ = conn.destroy_window(win);
                    let _ = conn.flush();
                    let _ = ready_tx.send(Err(anyhow::anyhow!("无法获取 CLIPBOARD ownership")));
                    anyhow::bail!("无法获取 CLIPBOARD ownership");
                }

                let _ = ready_tx.send(Ok(()));

                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);

                while std::time::Instant::now() < deadline {
                    if let Ok(Some(event)) = conn.poll_for_event() {
                        match event {
                            Event::SelectionRequest(req) => {
                                let mut reply = SelectionNotifyEvent {
                                    response_type: xproto::SELECTION_NOTIFY_EVENT,
                                    sequence: 0,
                                    time: req.time,
                                    requestor: req.requestor,
                                    selection: req.selection,
                                    target: req.target,
                                    property: 0u32.into(),
                                };

                                if req.target == targets_atom {
                                    let targets =
                                        [targets_atom, utf8_string, AtomEnum::STRING.into()];
                                    let _ = conn.change_property32(
                                        PropMode::REPLACE,
                                        req.requestor,
                                        req.property,
                                        AtomEnum::ATOM,
                                        &targets,
                                    );
                                    reply.property = req.property;
                                } else if req.target == utf8_string
                                    || req.target == u32::from(AtomEnum::STRING)
                                {
                                    let _ = conn.change_property8(
                                        PropMode::REPLACE,
                                        req.requestor,
                                        req.property,
                                        utf8_string,
                                        text_owned.as_bytes(),
                                    );
                                    reply.property = req.property;
                                }

                                let _ = conn.send_event(
                                    false,
                                    req.requestor,
                                    EventMask::NO_EVENT,
                                    reply,
                                );
                                let _ = conn.flush();

                                if req.target == utf8_string
                                    || req.target == u32::from(AtomEnum::STRING)
                                {
                                    let extra_deadline = std::time::Instant::now()
                                        + std::time::Duration::from_millis(200);
                                    while std::time::Instant::now() < extra_deadline {
                                        if let Ok(Some(Event::SelectionRequest(req2))) =
                                            conn.poll_for_event()
                                        {
                                            let mut r2 = SelectionNotifyEvent {
                                                response_type: xproto::SELECTION_NOTIFY_EVENT,
                                                sequence: 0,
                                                time: req2.time,
                                                requestor: req2.requestor,
                                                selection: req2.selection,
                                                target: req2.target,
                                                property: 0u32.into(),
                                            };
                                            if req2.target == targets_atom {
                                                let targets = [
                                                    targets_atom,
                                                    utf8_string,
                                                    AtomEnum::STRING.into(),
                                                ];
                                                let _ = conn.change_property32(
                                                    PropMode::REPLACE,
                                                    req2.requestor,
                                                    req2.property,
                                                    AtomEnum::ATOM,
                                                    &targets,
                                                );
                                                r2.property = req2.property;
                                            } else if req2.target == utf8_string
                                                || req2.target == u32::from(AtomEnum::STRING)
                                            {
                                                let _ = conn.change_property8(
                                                    PropMode::REPLACE,
                                                    req2.requestor,
                                                    req2.property,
                                                    utf8_string,
                                                    text_owned.as_bytes(),
                                                );
                                                r2.property = req2.property;
                                            }
                                            let _ = conn.send_event(
                                                false,
                                                req2.requestor,
                                                EventMask::NO_EVENT,
                                                r2,
                                            );
                                            let _ = conn.flush();
                                        } else {
                                            std::thread::sleep(std::time::Duration::from_millis(
                                                10,
                                            ));
                                        }
                                    }
                                    break;
                                }
                            }
                            Event::SelectionClear(_) => break,
                            _ => {}
                        }
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }

                conn.destroy_window(win)?;
                conn.flush()?;
                Ok(())
            });

            match ready_rx.await {
                Ok(Ok(())) => {
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    self.key_combo("ctrl+v").await?;
                    handle.await??;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    return Ok(());
                }
                Ok(Err(err)) => {
                    last_error = Some(err);
                    let _ = handle.await;
                }
                Err(_) => {
                    last_error = Some(anyhow::anyhow!("剪贴板同步通道已关闭"));
                    let _ = handle.await;
                }
            }

            if attempt < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("剪贴板粘贴失败")))
    }

    /// 通过剪贴板粘贴图片文件 (xclip + Ctrl+V)
    pub async fn paste_image(&mut self, image_path: &str) -> Result<()> {
        info!("🖼️ 粘贴图片: {}", image_path);

        // 检测 MIME 类型
        let mime = if image_path.ends_with(".png") {
            "image/png"
        } else if image_path.ends_with(".jpg") || image_path.ends_with(".jpeg") {
            "image/jpeg"
        } else if image_path.ends_with(".gif") {
            "image/gif"
        } else if image_path.ends_with(".bmp") {
            "image/bmp"
        } else {
            "image/png" // 默认 PNG
        };

        // xclip -selection clipboard -t image/png -i /path/to/image (异步)
        let status = tokio::process::Command::new("xclip")
            .args(["-selection", "clipboard", "-t", mime, "-i", image_path])
            .status()
            .await
            .context("启动 xclip 失败 (图片)")?;

        if !status.success() {
            anyhow::bail!("xclip 图片复制失败: exit={:?}", status.code());
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Ctrl+V 粘贴
        self.key_combo("ctrl+v").await?;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        Ok(())
    }

    // =================================================================
    // 鼠标操作
    // =================================================================

    /// 鼠标移动到绝对坐标
    pub async fn move_mouse(&mut self, x: i32, y: i32) -> Result<()> {
        self.conn
            .xtest_fake_input(MOTION_NOTIFY, 0, 0, self.screen_root, x as i16, y as i16, 0)?;
        self.conn.flush()?;
        debug!("🖱️ move_mouse: ({x}, {y})");
        Ok(())
    }

    /// 鼠标单击
    pub async fn click(&mut self, x: i32, y: i32) -> Result<()> {
        self.move_mouse(x, y).await?;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // 按下左键
        self.conn
            .xtest_fake_input(BUTTON_PRESS, 1, 0, self.screen_root, 0, 0, 0)?;
        self.conn.flush()?;
        tokio::time::sleep(std::time::Duration::from_millis(CLICK_HOLD_MS)).await;

        // 释放左键
        self.conn
            .xtest_fake_input(BUTTON_RELEASE, 1, 0, self.screen_root, 0, 0, 0)?;
        self.conn.flush()?;

        debug!("🖱️ click: ({x}, {y})");
        Ok(())
    }

    /// 鼠标双击
    pub async fn double_click(&mut self, x: i32, y: i32) -> Result<()> {
        self.click(x, y).await?;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        self.click(x, y).await?;
        Ok(())
    }

    /// 鼠标右键点击
    #[allow(dead_code)]
    pub async fn right_click(&mut self, x: i32, y: i32) -> Result<()> {
        self.move_mouse(x, y).await?;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        self.conn
            .xtest_fake_input(BUTTON_PRESS, 3, 0, self.screen_root, 0, 0, 0)?;
        self.conn.flush()?;
        tokio::time::sleep(std::time::Duration::from_millis(CLICK_HOLD_MS)).await;

        self.conn
            .xtest_fake_input(BUTTON_RELEASE, 3, 0, self.screen_root, 0, 0, 0)?;
        self.conn.flush()?;

        debug!("🖱️ right_click: ({x}, {y})");
        Ok(())
    }

    /// 鼠标滚轮 (正=上, 负=下)
    ///
    /// X11: button 4 = scroll up, button 5 = scroll down
    #[allow(dead_code)]
    pub async fn scroll(&mut self, x: i32, y: i32, clicks: i32) -> Result<()> {
        self.move_mouse(x, y).await?;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let button: u8 = if clicks > 0 { 4 } else { 5 };
        for _ in 0..clicks.unsigned_abs() {
            self.conn
                .xtest_fake_input(BUTTON_PRESS, button, 0, self.screen_root, 0, 0, 0)?;
            self.conn
                .xtest_fake_input(BUTTON_RELEASE, button, 0, self.screen_root, 0, 0, 0)?;
            self.conn.flush()?;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        debug!("🖱️ scroll: ({x}, {y}) clicks={clicks}");
        Ok(())
    }

    // =================================================================
    // 窗口管理 (X11 原生, 替代 xdotool)
    // =================================================================

    /// 按标题搜索窗口 (EWMH _NET_CLIENT_LIST + 标题匹配)
    ///
    /// `exact=true`: 精确匹配; `exact=false`: contains 匹配
    /// 返回匹配的 (window_id, window_name) 列表
    pub fn find_windows_by_title(&self, title: &str, exact: bool) -> Result<Vec<(u32, String)>> {
        // 使用缓存的 Atom (启动时已 intern)
        let wm_name_atom = self.atom_net_wm_name;
        let utf8_atom = self.atom_utf8_string;
        let client_list_atom = self.atom_net_client_list;

        // 优先: _NET_CLIENT_LIST (WM 托管的所有顶层窗口)
        let windows: Vec<u32> = if let Ok(reply) = self
            .conn
            .get_property(
                false,
                self.screen_root,
                client_list_atom,
                u32::from(AtomEnum::WINDOW),
                0,
                4096,
            )?
            .reply()
        {
            if reply.format == 32 && !reply.value.is_empty() {
                reply
                    .value
                    .chunks_exact(4)
                    .map(|chunk| u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                    .collect()
            } else {
                // 回退: query_tree
                self.conn.query_tree(self.screen_root)?.reply()?.children
            }
        } else {
            self.conn.query_tree(self.screen_root)?.reply()?.children
        };

        let mut found = Vec::new();

        for &win in &windows {
            // 尝试 _NET_WM_NAME (UTF-8), 回退 WM_NAME
            let name = if let Ok(reply) = self
                .conn
                .get_property(false, win, wm_name_atom, utf8_atom, 0, 1024)?
                .reply()
            {
                if reply.value.is_empty() {
                    if let Ok(reply2) = self
                        .conn
                        .get_property(
                            false,
                            win,
                            u32::from(AtomEnum::WM_NAME),
                            u32::from(AtomEnum::STRING),
                            0,
                            1024,
                        )?
                        .reply()
                    {
                        String::from_utf8_lossy(&reply2.value).to_string()
                    } else {
                        continue;
                    }
                } else {
                    String::from_utf8_lossy(&reply.value).to_string()
                }
            } else {
                continue;
            };

            let matched = if exact {
                name == title
            } else {
                name.contains(title)
            };
            if matched {
                found.push((win, name));
            }
        }
        Ok(found)
    }

    /// 通过窗口标题激活指定窗口 (X11 _NET_ACTIVE_WINDOW)
    ///
    /// 返回是否成功找到并激活了窗口
    pub fn activate_window_by_title(&self, title: &str, exact: bool) -> Result<bool> {
        let windows = self.find_windows_by_title(title, exact)?;
        if let Some((win, name)) = windows.first() {
            debug!("🖱️ 激活窗口: '{name}' (wid={win})");
            let active_atom = self.atom_net_active_window;
            // _NET_ACTIVE_WINDOW: data[0]=source(1=app), data[1]=timestamp, data[2]=requestor
            let event = ClientMessageEvent {
                response_type: xproto::CLIENT_MESSAGE_EVENT,
                format: 32,
                sequence: 0,
                window: *win,
                type_: active_atom,
                data: [1u32, 0, 0, 0, 0].into(),
            };
            self.conn.send_event(
                false,
                self.screen_root,
                EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
                event,
            )?;
            self.conn.flush()?;
            Ok(true)
        } else {
            debug!("🖱️ 未找到标题匹配 '{title}' 的窗口");
            Ok(false)
        }
    }

    /// 当前活动窗口标题
    pub fn active_window_title(&self) -> Result<Option<String>> {
        let reply = self
            .conn
            .get_property(
                false,
                self.screen_root,
                self.atom_net_active_window,
                u32::from(AtomEnum::WINDOW),
                0,
                1,
            )?
            .reply()?;

        if reply.format != 32 || reply.value.len() < 4 {
            return Ok(None);
        }

        let win = u32::from_ne_bytes([
            reply.value[0],
            reply.value[1],
            reply.value[2],
            reply.value[3],
        ]);
        if win == 0 {
            return Ok(None);
        }

        self.window_title(win)
    }

    pub fn active_window_contains(&self, title: &str) -> Result<bool> {
        Ok(self
            .active_window_title()?
            .map(|current| current.contains(title))
            .unwrap_or(false))
    }

    fn window_title(&self, win: u32) -> Result<Option<String>> {
        let reply = self
            .conn
            .get_property(
                false,
                win,
                self.atom_net_wm_name,
                self.atom_utf8_string,
                0,
                1024,
            )?
            .reply()?;
        if !reply.value.is_empty() {
            return Ok(Some(String::from_utf8_lossy(&reply.value).to_string()));
        }

        let fallback = self
            .conn
            .get_property(
                false,
                win,
                u32::from(AtomEnum::WM_NAME),
                u32::from(AtomEnum::STRING),
                0,
                1024,
            )?
            .reply()?;
        if fallback.value.is_empty() {
            Ok(None)
        } else {
            Ok(Some(String::from_utf8_lossy(&fallback.value).to_string()))
        }
    }

    /// 通过窗口标题关闭指定窗口 (X11 _NET_CLOSE_WINDOW)
    pub fn close_window_by_title(&self, title: &str) -> Result<bool> {
        let windows = self.find_windows_by_title(title, false)?;
        if let Some((win, name)) = windows.first() {
            info!("🗑️ 关闭窗口: '{name}' (匹配 '{title}')");
            let close_atom = self.atom_net_close_window;
            let event = ClientMessageEvent {
                response_type: xproto::CLIENT_MESSAGE_EVENT,
                format: 32,
                sequence: 0,
                window: *win,
                type_: close_atom,
                data: [0u32; 5].into(),
            };
            self.conn.send_event(
                false,
                self.screen_root,
                EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
                event,
            )?;
            self.conn.flush()?;
            Ok(true)
        } else {
            debug!("🗑️ 未找到标题包含 '{title}' 的窗口");
            Ok(false)
        }
    }

    /// 发送 Enter 键
    pub async fn press_enter(&mut self) -> Result<()> {
        self.press_key("Return").await
    }
}

/// 截取所有微信窗口并横向拼接为 PNG
///
/// 主窗口排最左, 独立聊天窗口依次排列在右侧。
/// 找不到任何微信窗口时截取整个屏幕。
/// 独立于 InputEngine, 使用临时 X11 连接, 不阻塞输入 actor.
pub fn capture_screenshot() -> Result<Vec<u8>> {
    let display_env = std::env::var("DISPLAY").unwrap_or_else(|_| ":1".into());
    let (conn, screen_num) = RustConnection::connect(Some(&display_env))
        .context(format!("截屏连接 X11 失败 (DISPLAY={display_env})"))?;

    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let depth = screen.root_depth;
    let bpp = if depth >= 24 { 4usize } else { 4usize };

    let wechat_wins = find_wechat_windows(&conn, root);

    // 截取每个窗口的 RGB 数据 + 尺寸
    let mut captures: Vec<(Vec<u8>, u32, u32)> = Vec::new(); // (rgb, w, h)

    if wechat_wins.is_empty() {
        // 没有微信窗口, 截整个屏幕
        debug!("📸 未找到微信窗口, 截取整个屏幕");
        let w = screen.width_in_pixels;
        let h = screen.height_in_pixels;
        let image = conn
            .get_image(xproto::ImageFormat::Z_PIXMAP, root, 0, 0, w, h, !0)?
            .reply()
            .context("X11 GetImage 失败")?;
        captures.push((bgr_to_rgb(&image.data, w as u32, h as u32, bpp), w as u32, h as u32));
    } else {
        for (win, name, w, h) in &wechat_wins {
            if let Some(image) = conn
                .get_image(xproto::ImageFormat::Z_PIXMAP, *win, 0, 0, *w, *h, !0)
                .ok()
                .and_then(|c| c.reply().ok())
            {
                debug!("📸 截取窗口: '{}' 0x{:x} {}x{}", name, win, w, h);
                captures.push((bgr_to_rgb(&image.data, *w as u32, *h as u32, bpp), *w as u32, *h as u32));
            } else {
                debug!("📸 截取窗口失败: '{}'", name);
            }
        }
    }

    if captures.is_empty() {
        anyhow::bail!("没有可截取的窗口");
    }

    // 横向拼接: 总宽 = sum(w), 总高 = max(h)
    let total_w: u32 = captures.iter().map(|(_, w, _)| w).sum();
    let total_h: u32 = captures.iter().map(|(_, _, h)| *h).max().unwrap();

    let mut canvas = vec![0u8; (total_w * total_h * 3) as usize];
    let mut x_offset: u32 = 0;

    for (rgb, w, h) in &captures {
        for y in 0..*h {
            let src_start = (y * w * 3) as usize;
            let src_end = src_start + (*w * 3) as usize;
            let dst_start = ((y * total_w + x_offset) * 3) as usize;
            if src_end <= rgb.len() {
                canvas[dst_start..dst_start + (*w * 3) as usize]
                    .copy_from_slice(&rgb[src_start..src_end]);
            }
        }
        x_offset += w;
    }

    // 编码为 PNG
    let mut png_buf = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_buf, total_w, total_h);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().context("PNG header 写入失败")?;
        writer.write_image_data(&canvas).context("PNG 数据写入失败")?;
    }

    debug!(
        "📸 截屏完成: {}x{} ({} 个窗口) depth={} → {} bytes PNG",
        total_w, total_h, captures.len(), depth, png_buf.len()
    );
    Ok(png_buf)
}

/// X11 BGRx/BGRA → RGB
fn bgr_to_rgb(data: &[u8], w: u32, h: u32, bpp: usize) -> Vec<u8> {
    let pixel_count = (w * h) as usize;
    let mut rgb = Vec::with_capacity(pixel_count * 3);
    for i in 0..pixel_count {
        let offset = i * bpp;
        if offset + 2 < data.len() {
            rgb.push(data[offset + 2]); // R
            rgb.push(data[offset + 1]); // G
            rgb.push(data[offset]);      // B
        } else {
            rgb.extend_from_slice(&[0, 0, 0]);
        }
    }
    rgb
}

/// 查找所有微信窗口, 主窗口排第一
/// 返回 Vec<(window_id, title, width, height)>
fn find_wechat_windows(
    conn: &RustConnection,
    root: u32,
) -> Vec<(u32, String, u16, u16)> {
    let atoms = match intern_screenshot_atoms(conn) {
        Some(a) => a,
        None => return vec![],
    };

    let windows = match get_client_list(conn, root, atoms.0) {
        Some(w) => w,
        None => return vec![],
    };

    let mut main_win: Option<(u32, String, u16, u16)> = None;
    let mut other_wins: Vec<(u32, String, u16, u16)> = Vec::new();

    for &win in &windows {
        let name = get_window_name(conn, win, atoms.1, atoms.2).unwrap_or_default();
        let is_exact = name == "微信" || name == "WeChat";
        let is_wechat = is_exact || name.contains("微信") || name.contains("WeChat");

        if !is_wechat {
            continue;
        }

        let geo = match conn.get_geometry(win).ok().and_then(|c| c.reply().ok()) {
            Some(g) if g.width > 0 && g.height > 0 => g,
            _ => continue,
        };

        if is_exact && main_win.is_none() {
            main_win = Some((win, name, geo.width, geo.height));
        } else {
            other_wins.push((win, name, geo.width, geo.height));
        }
    }

    let mut result = Vec::new();
    if let Some(m) = main_win {
        result.push(m);
    }
    result.extend(other_wins);
    result
}

/// intern 截屏所需的 X11 Atom: (_NET_CLIENT_LIST, _NET_WM_NAME, UTF8_STRING)
fn intern_screenshot_atoms(conn: &RustConnection) -> Option<(u32, u32, u32)> {
    let a = conn.intern_atom(false, b"_NET_CLIENT_LIST").ok()?.reply().ok()?.atom;
    let b = conn.intern_atom(false, b"_NET_WM_NAME").ok()?.reply().ok()?.atom;
    let c = conn.intern_atom(false, b"UTF8_STRING").ok()?.reply().ok()?.atom;
    Some((a, b, c))
}

/// 从 _NET_CLIENT_LIST 获取所有顶层窗口
fn get_client_list(conn: &RustConnection, root: u32, atom: u32) -> Option<Vec<u32>> {
    let reply = conn
        .get_property(false, root, atom, u32::from(AtomEnum::WINDOW), 0, 4096)
        .ok()?
        .reply()
        .ok()?;
    if reply.format == 32 && !reply.value.is_empty() {
        Some(
            reply.value
                .chunks_exact(4)
                .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
        )
    } else {
        None
    }
}

fn get_window_name(
    conn: &RustConnection,
    win: u32,
    net_wm_name: u32,
    utf8_string: u32,
) -> Option<String> {
    // 尝试 _NET_WM_NAME
    if let Ok(reply) = conn
        .get_property(false, win, net_wm_name, utf8_string, 0, 1024)
        .ok()?
        .reply()
    {
        if !reply.value.is_empty() {
            return Some(String::from_utf8_lossy(&reply.value).to_string());
        }
    }
    // 回退 WM_NAME
    if let Ok(reply) = conn
        .get_property(
            false,
            win,
            u32::from(AtomEnum::WM_NAME),
            u32::from(AtomEnum::STRING),
            0,
            1024,
        )
        .ok()?
        .reply()
    {
        if !reply.value.is_empty() {
            return Some(String::from_utf8_lossy(&reply.value).to_string());
        }
    }
    None
}
