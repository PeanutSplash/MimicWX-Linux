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
        debug!("初始化 X11 XTEST 输入引擎...");

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

        debug!("X11 XTEST 就绪 (DISPLAY={display_env}, keycodes={min_keycode}~{max_keycode})");

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
        debug!("粘贴文本: {} 字符", text.len());
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
        debug!("粘贴图片: {}", image_path);

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
            debug!("关闭窗口: '{name}' (匹配 '{title}')");
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
        captures.push((
            bgr_to_rgb(&image.data, w as u32, h as u32, bpp),
            w as u32,
            h as u32,
        ));
    } else {
        for (win, name, w, h) in &wechat_wins {
            if let Some(image) = conn
                .get_image(xproto::ImageFormat::Z_PIXMAP, *win, 0, 0, *w, *h, !0)
                .ok()
                .and_then(|c| c.reply().ok())
            {
                debug!("📸 截取窗口: '{}' 0x{:x} {}x{}", name, win, w, h);
                captures.push((
                    bgr_to_rgb(&image.data, *w as u32, *h as u32, bpp),
                    *w as u32,
                    *h as u32,
                ));
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
        writer
            .write_image_data(&canvas)
            .context("PNG 数据写入失败")?;
    }

    debug!(
        "📸 截屏完成: {}x{} ({} 个窗口) depth={} → {} bytes PNG",
        total_w,
        total_h,
        captures.len(),
        depth,
        png_buf.len()
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
            rgb.push(data[offset]); // B
        } else {
            rgb.extend_from_slice(&[0, 0, 0]);
        }
    }
    rgb
}

/// 查找所有微信窗口, 最大窗口 (主窗口) 排第一
/// 返回 Vec<(window_id, title, width, height)>
///
/// 匹配策略:
/// - _NET_WM_NAME 或 WM_CLASS 含 "wechat" (不区分大小写)
/// - 包含 "WeChatAppEx" (微信内嵌浏览器)
/// - 同进程的无名窗口 (通过 _NET_WM_PID 关联)
/// - 过滤掉面积过小的辅助窗口 (< 50x50)
fn find_wechat_windows(conn: &RustConnection, root: u32) -> Vec<(u32, String, u16, u16)> {
    let atoms = match intern_screenshot_atoms(conn) {
        Some(a) => a,
        None => return vec![],
    };

    let wm_class_atom = conn
        .intern_atom(false, b"WM_CLASS")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom)
        .unwrap_or(0);
    let wm_pid_atom = conn
        .intern_atom(false, b"_NET_WM_PID")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom)
        .unwrap_or(0);

    // ① _NET_CLIENT_LIST: WM 管理的客户端窗口 (有属性)
    let client_list = get_client_list(conn, root, atoms.0).unwrap_or_default();

    // 从 client list 中找微信窗口, 同时收集 PID
    let mut wechat_pids = std::collections::HashSet::new();
    let mut wins: Vec<(u32, String, u16, u16)> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for &win in &client_list {
        let name = get_window_name(conn, win, atoms.1, atoms.2).unwrap_or_default();
        let class = get_wm_class(conn, win, wm_class_atom).unwrap_or_default();

        if !is_wechat_name(&name) && !is_wechat_name(&class) {
            continue;
        }

        if let Some(pid) = get_window_pid(conn, win, wm_pid_atom) {
            wechat_pids.insert(pid);
        }

        let geo = match conn.get_geometry(win).ok().and_then(|c| c.reply().ok()) {
            Some(g) if g.width >= 50 && g.height >= 50 => g,
            _ => continue,
        };

        let label = if !name.is_empty() { name } else { class };
        wins.push((win, label, geo.width, geo.height));
        seen.insert(win);
    }

    // ② query_tree: 补充无名/未注册的窗口 (通过同 PID 关联)
    if !wechat_pids.is_empty() {
        if let Some(tree) = conn.query_tree(root).ok().and_then(|c| c.reply().ok()) {
            for &win in &tree.children {
                if seen.contains(&win) {
                    continue;
                }
                let pid = get_window_pid(conn, win, wm_pid_atom);
                if !pid.map(|p| wechat_pids.contains(&p)).unwrap_or(false) {
                    continue;
                }
                let geo = match conn.get_geometry(win).ok().and_then(|c| c.reply().ok()) {
                    Some(g) if g.width >= 50 && g.height >= 50 => g,
                    _ => continue,
                };
                let name = get_window_name(conn, win, atoms.1, atoms.2)
                    .unwrap_or_else(|| format!("0x{:x}", win));
                wins.push((win, name, geo.width, geo.height));
            }
        }
    }

    // 按面积降序排列, 最大窗口 (主窗口) 在前
    wins.sort_by(|a, b| {
        let area_a = (a.2 as u32) * (a.3 as u32);
        let area_b = (b.2 as u32) * (b.3 as u32);
        area_b.cmp(&area_a)
    });

    wins
}

fn is_wechat_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("wechat") || name.contains("微信")
}

/// intern 截屏所需的 X11 Atom: (_NET_CLIENT_LIST, _NET_WM_NAME, UTF8_STRING)
fn intern_screenshot_atoms(conn: &RustConnection) -> Option<(u32, u32, u32)> {
    let a = conn
        .intern_atom(false, b"_NET_CLIENT_LIST")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let b = conn
        .intern_atom(false, b"_NET_WM_NAME")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let c = conn
        .intern_atom(false, b"UTF8_STRING")
        .ok()?
        .reply()
        .ok()?
        .atom;
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
            reply
                .value
                .chunks_exact(4)
                .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
        )
    } else {
        None
    }
}

/// 获取 WM_CLASS (格式: "instance\0class")
fn get_wm_class(conn: &RustConnection, win: u32, wm_class_atom: u32) -> Option<String> {
    if wm_class_atom == 0 {
        return None;
    }
    let reply = conn
        .get_property(
            false,
            win,
            wm_class_atom,
            u32::from(AtomEnum::STRING),
            0,
            256,
        )
        .ok()?
        .reply()
        .ok()?;
    if reply.value.is_empty() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&reply.value)
            .replace('\0', " ")
            .trim()
            .to_string(),
    )
}

/// 获取 _NET_WM_PID
fn get_window_pid(conn: &RustConnection, win: u32, pid_atom: u32) -> Option<u32> {
    if pid_atom == 0 {
        return None;
    }
    let reply = conn
        .get_property(false, win, pid_atom, u32::from(AtomEnum::CARDINAL), 0, 1)
        .ok()?
        .reply()
        .ok()?;
    if reply.format == 32 && reply.value.len() >= 4 {
        Some(u32::from_ne_bytes([
            reply.value[0],
            reply.value[1],
            reply.value[2],
            reply.value[3],
        ]))
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

// =====================================================================
// QR 码检测 + 终端渲染
// =====================================================================

/// 从微信窗口截屏中检测二维码, 返回二维码内容字符串
pub fn detect_qr_from_screenshot() -> Option<String> {
    let display_env = std::env::var("DISPLAY").unwrap_or_else(|_| ":1".into());
    let (conn, screen_num) = RustConnection::connect(Some(&display_env)).ok()?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    // 只截主窗口 (二维码在主窗口上)
    let wins = find_wechat_windows(&conn, root);
    let (win, w, h) = if let Some((id, _, w, h)) = wins.first() {
        (*id, *w, *h)
    } else {
        (root, screen.width_in_pixels, screen.height_in_pixels)
    };

    let image = conn
        .get_image(xproto::ImageFormat::Z_PIXMAP, win, 0, 0, w, h, !0)
        .ok()?
        .reply()
        .ok()?;

    let bpp = 4usize;
    let pixel_count = (w as usize) * (h as usize);

    // BGR(A) → grayscale
    let mut gray = Vec::with_capacity(pixel_count);
    for i in 0..pixel_count {
        let offset = i * bpp;
        if offset + 2 < image.data.len() {
            let b = image.data[offset] as u32;
            let g = image.data[offset + 1] as u32;
            let r = image.data[offset + 2] as u32;
            gray.push(((r * 299 + g * 587 + b * 114) / 1000) as u8);
        } else {
            gray.push(0);
        }
    }

    // rqrr 检测
    let mut img = rqrr::PreparedImage::prepare_from_greyscale(w as usize, h as usize, |x, y| {
        gray[y * (w as usize) + x]
    });
    let grids = img.detect_grids();
    for grid in grids {
        if let Ok((_meta, content)) = grid.decode() {
            return Some(content);
        }
    }
    None
}

/// 将二维码内容渲染为终端字符串 (Unicode half-block)
/// 返回 (渲染后的字符串, 行数)
pub fn render_qr_to_terminal(content: &str) -> Option<(String, usize)> {
    use qrcode::QrCode;

    let code = QrCode::new(content.as_bytes()).ok()?;
    let modules = code.to_colors();
    let width = code.width();

    // 使用 Unicode half-block 渲染, 每个字符表示上下两个模块
    // 终端通常深色背景, 所以: 黑模块=背景色, 白模块=前景色
    // ▀ = 上半块, ▄ = 下半块, █ = 全块, ' ' = 空
    let mut lines: Vec<String> = Vec::new();

    // 加 2 格白色 quiet zone
    let qz = 2;
    let total_w = width + qz * 2;

    // 顶部 quiet zone (1行 = 2行模块)
    lines.push(format!("  {}", "█".repeat(total_w)));

    let rows: Vec<&[qrcode::Color]> = modules.chunks(width).collect();
    let mut y = 0;
    while y < rows.len() {
        let mut line = String::from("  ");
        // 左 quiet zone
        for _ in 0..qz {
            line.push('█');
        }
        for x in 0..width {
            let top = rows[y][x];
            let bottom = if y + 1 < rows.len() {
                rows[y + 1][x]
            } else {
                qrcode::Color::Light
            };
            match (top, bottom) {
                (qrcode::Color::Dark, qrcode::Color::Dark) => line.push(' '),
                (qrcode::Color::Dark, qrcode::Color::Light) => line.push('▄'),
                (qrcode::Color::Light, qrcode::Color::Dark) => line.push('▀'),
                (qrcode::Color::Light, qrcode::Color::Light) => line.push('█'),
            }
        }
        // 右 quiet zone
        for _ in 0..qz {
            line.push('█');
        }
        lines.push(line);
        y += 2;
    }

    // 底部 quiet zone
    lines.push(format!("  {}", "█".repeat(total_w)));

    let line_count = lines.len();
    Some((lines.join("\n"), line_count))
}
