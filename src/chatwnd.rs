//! 独立聊天窗口 (ChatWnd)
//!
//! 借鉴 wxauto 的 ChatWnd 设计：每个独立弹出的聊天窗口拥有自己的
//! AT-SPI2 节点引用，可以独立读取消息和发送，互不干扰。
//!
//! 使用方式 (对应 wxauto):
//!   wxauto: wx.AddListenChat("张三") → 弹出独立窗口 → ChatWnd("张三")
//!   MimicWX: POST /listen {"who":"张三"} → 双击弹出 → ChatWnd 实例化

use anyhow::Result;
use std::sync::Arc;
use tracing::{debug, warn};

use crate::atspi::{AtSpi, NodeRef, SearchAction};
use crate::input::InputEngine;
use crate::node_handle::{NameMatch, NodeFingerprint, NodeHandle};
use crate::wechat::ms;

// =====================================================================
// ChatWnd — 独立聊天窗口
// =====================================================================

pub struct ChatWnd {
    /// 聊天对象名称
    pub who: String,
    /// AT-SPI2 引用
    atspi: Arc<AtSpi>,
    /// 该窗口的 AT-SPI2 根节点 (frame)
    window_node: NodeHandle,
    /// 缓存的输入框节点
    edit_box_node: NodeHandle,
    /// 缓存的消息列表节点
    msg_list_node: NodeHandle,
}

impl ChatWnd {
    /// 创建独立聊天窗口实例
    ///
    /// `window_node` 应该是 AT-SPI2 树中该独立窗口的 frame 节点
    pub fn new(who: String, atspi: Arc<AtSpi>, search_root: NodeRef, window_node: NodeRef) -> Self {
        debug!("ChatWnd 创建: {who}");
        let window_fp = NodeFingerprint::new(["frame"], NameMatch::Contains(who.clone()));
        let edit_fp = NodeFingerprint::new(["entry", "text"], NameMatch::Any);
        let msg_list_fp = NodeFingerprint::new(
            ["list"],
            NameMatch::AnyOf(vec![
                NameMatch::Contains("消息".into()),
                NameMatch::Contains("Messages".into()),
                NameMatch::Contains("Message".into()),
            ]),
        );
        Self {
            who,
            atspi,
            window_node: NodeHandle::with_current(search_root, window_fp, window_node.clone()),
            edit_box_node: NodeHandle::new(window_node.clone(), edit_fp),
            msg_list_node: NodeHandle::new(window_node, msg_list_fp),
        }
    }

    /// 刷新窗口节点引用 (窗口可能被重新创建)
    #[allow(dead_code)]
    pub fn update_window_node(&mut self, node: NodeRef) {
        self.window_node.rebind(node.clone());
        self.edit_box_node.set_search_root(node.clone());
        self.msg_list_node.set_search_root(node);
        self.edit_box_node.invalidate();
        self.msg_list_node.invalidate();
    }

    /// 检查独立窗口是否仍然存活
    /// 通过 AT-SPI2 bbox 是否返回有效值来判断
    pub async fn is_alive(&self) -> bool {
        self.window_node.is_valid(&self.atspi).await
    }

    /// 初始化输入框缓存 (DFS 搜索, 只跑一次)
    ///
    /// 不限制结构性角色, 遍历所有子节点找 `entry`/`text`
    pub async fn init_edit_box(&mut self) {
        if self.edit_box_node.is_valid(&self.atspi).await {
            return; // 已缓存
        }
        let Some(win) = self.window_node.resolve(&self.atspi).await else {
            return;
        };
        self.edit_box_node.set_search_root(win.clone());
        if let Some(node) = self
            .atspi
            .find_dfs(
                &win,
                &|role, _| {
                    if role == "entry" || role == "text" {
                        SearchAction::Found
                    } else if role == "list" {
                        SearchAction::Skip // 跳过消息列表
                    } else {
                        SearchAction::Recurse
                    }
                },
                0,
                15,
                30,
            )
            .await
        {
            debug!("[ChatWnd] 缓存输入框: {}", self.who);
            self.edit_box_node.rebind(node);
        } else {
            debug!("[ChatWnd] 未找到输入框, 使用偏移量方案: {}", self.who);
        }
    }

    /// 初始化消息列表缓存 (DFS 搜索, 只跑一次)
    pub async fn init_msg_list(&mut self) {
        if self.msg_list_node.is_valid(&self.atspi).await {
            return;
        }
        let Some(win) = self.window_node.resolve(&self.atspi).await else {
            return;
        };
        self.msg_list_node.set_search_root(win.clone());
        if let Some(node) = self
            .atspi
            .find_dfs(
                &win,
                &|role, name| {
                    if role == "list"
                        && (name.contains("消息")
                            || name.contains("Messages")
                            || name.contains("Message"))
                    {
                        SearchAction::Found
                    } else if role == "list" {
                        SearchAction::Skip // 跳过其他 list
                    } else {
                        SearchAction::Recurse
                    }
                },
                0,
                15,
                30,
            )
            .await
        {
            debug!("[ChatWnd] 缓存消息列表: {}", self.who);
            self.msg_list_node.rebind(node);
        } else {
            warn!("[ChatWnd] 未找到消息列表: {}", self.who);
        }
    }

    // =================================================================
    // 消息列表
    // =================================================================

    /// 在此独立窗口中查找消息列表
    pub async fn find_message_list(&self) -> Option<NodeRef> {
        let mut handle = self.msg_list_node.clone();
        handle.resolve(&self.atspi).await
    }

    /// 在此独立窗口中查找输入框
    #[allow(dead_code)]
    pub async fn find_edit_box(&self) -> Option<NodeRef> {
        let mut handle = self.edit_box_node.clone();
        handle.resolve(&self.atspi).await
    }

    // =================================================================
    // 发送消息
    // =================================================================

    /// 在此独立窗口中发送消息
    ///
    /// 简化流程: 点击窗口聚焦 → 粘贴 → Enter
    /// (独立聊天窗口会自动聚焦输入框)
    pub async fn send_message(
        &mut self,
        engine: &mut InputEngine,
        text: &str,
        skip_verify: bool,
    ) -> Result<(bool, bool, String)> {
        debug!("[ChatWnd] 发送: [{}] text_len={}", self.who, text.len());

        // 1. 激活窗口并聚焦输入框
        self.activate_and_focus_input(engine).await?;

        // 2. 粘贴消息 (X11 Selection + Ctrl+V)
        engine.paste_text(text).await?;
        tokio::time::sleep(ms(300)).await;

        // 3. Enter 发送
        engine.press_enter().await?;
        tokio::time::sleep(ms(500)).await;

        // 4. 验证发送 (可跳过, 由 API 层的 DB 验证替代)
        let verified = if skip_verify {
            debug!(
                "⏩ [ChatWnd] 跳过 AT-SPI 验证 (将由 DB 验证): [{}]",
                self.who
            );
            false
        } else {
            self.verify_sent(text).await
        };

        let msg = if verified {
            "消息已发送"
        } else {
            "消息已发送 (未验证)"
        };
        debug!("[ChatWnd] 发送完成: [{}] verified={verified}", self.who);
        Ok((true, verified, msg.into()))
    }

    /// 在此独立窗口中发送图片
    ///
    /// 流程: 激活窗口 → 点击输入框 → 粘贴图片 → Enter
    /// (图片不做文本验证)
    pub async fn send_image(
        &mut self,
        engine: &mut InputEngine,
        image_path: &str,
    ) -> Result<(bool, bool, String)> {
        debug!("[ChatWnd] 发送图片: [{}] → {image_path}", self.who);

        // 1. 激活窗口并聚焦输入框
        self.activate_and_focus_input(engine).await?;

        // 2. 粘贴图片
        engine.paste_image(image_path).await?;
        tokio::time::sleep(ms(500)).await;

        // 3. Enter 发送
        engine.press_enter().await?;
        tokio::time::sleep(ms(500)).await;

        debug!("[ChatWnd] 图片发送完成: [{}]", self.who);
        Ok((true, false, "图片已发送 (独立窗口)".into()))
    }

    /// 激活独立窗口并聚焦输入框 (send_message/send_image/@ 的公共前置步骤)
    pub async fn activate_and_focus_input(&mut self, engine: &mut InputEngine) -> Result<()> {
        // 1. 将独立窗口提到前台 (X11 _NET_ACTIVE_WINDOW)
        let activated = engine
            .activate_window_by_title(&self.who, false)
            .unwrap_or(false);
        if !activated {
            // 回退: 点击标题栏
            if let Some(window_node) = self.window_node.resolve(&self.atspi).await {
                self.edit_box_node.set_search_root(window_node.clone());
                self.msg_list_node.set_search_root(window_node.clone());
                if let Some(bbox) = self.atspi.bbox(&window_node).await {
                    let cx = bbox.x + bbox.w / 2;
                    engine.click(cx, bbox.y + 30).await?;
                }
            }
        }
        tokio::time::sleep(ms(300)).await;

        if !engine.active_window_contains(&self.who).unwrap_or(false) {
            anyhow::bail!("focus lost: 独立窗口未激活 {}", self.who);
        }

        if !self.edit_box_node.is_valid(&self.atspi).await {
            if self.find_edit_box().await.is_some() {
                debug!("🔄 [ChatWnd] 输入框缓存失效, 重新搜索: {}", self.who);
            }
            self.edit_box_node.invalidate();
            self.init_edit_box().await;
        }

        if let Some(edit_node) = self.edit_box_node.resolve(&self.atspi).await {
            if let Some(eb) = self.atspi.bbox(&edit_node).await {
                let (cx, cy) = eb.center();
                engine.click(cx, cy).await?;
                tokio::time::sleep(ms(200)).await;
                return Ok(());
            }
        }

        if let Some(window_node) = self.window_node.resolve(&self.atspi).await {
            if let Some(bbox) = self.atspi.bbox(&window_node).await {
                let cx = bbox.x + bbox.w / 2;
                engine.click(cx, bbox.y + bbox.h - 50).await?;
                tokio::time::sleep(ms(200)).await;
            }
        }

        Ok(())
    }

    /// 验证消息是否出现在消息列表末尾
    async fn verify_sent(&mut self, text: &str) -> bool {
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(ms(500)).await;
            }

            if !self.msg_list_node.is_valid(&self.atspi).await {
                debug!("🔄 [ChatWnd] 消息列表缓存失效, 重新搜索: {}", self.who);
                self.msg_list_node.invalidate();
                self.init_msg_list().await;
            }

            let msg_list = if let Some(cached) = self.msg_list_node.resolve(&self.atspi).await {
                cached
            } else {
                match self.find_message_list().await {
                    Some(l) => l,
                    None => continue,
                }
            };
            if crate::wechat::verify_sent_in_list(&self.atspi, &msg_list, text, attempt).await {
                return true;
            }
        }
        false
    }

    pub async fn verify_sent_public(&mut self, text: &str) -> bool {
        self.verify_sent(text).await
    }
}
