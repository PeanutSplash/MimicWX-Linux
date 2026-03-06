use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::api::InputCommand;
use crate::db::{DbManager, DbMessage};

#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)]
pub struct SendResult {
    pub sent: bool,
    pub verified: bool,
    pub message: String,
}

#[async_trait::async_trait]
pub trait KeyProvider: Send + Sync {
    async fn get_key(&self) -> Result<String>;
    fn is_ready(&self) -> bool;
}

#[async_trait::async_trait]
#[allow(dead_code)]
pub trait MessageSource: Send + Sync {
    async fn get_new_messages(&self) -> Result<Vec<DbMessage>>;
    fn spawn_watcher(&self) -> broadcast::Receiver<()>;
}

#[async_trait::async_trait]
#[allow(dead_code)]
pub trait MessageSender: Send + Sync {
    async fn send_message(&self, to: &str, text: &str, at: &[String]) -> Result<SendResult>;
    async fn send_image(&self, to: &str, image_path: &str) -> Result<SendResult>;
}

#[async_trait::async_trait]
#[allow(dead_code)]
pub trait SessionLocator: Send + Sync {
    async fn chat_with(&self, who: &str) -> Result<Option<String>>;
    async fn add_listen(&self, who: &str) -> Result<bool>;
    async fn remove_listen(&self, who: &str) -> bool;
}

pub struct GdbFileKeyProvider {
    path: PathBuf,
}

impl GdbFileKeyProvider {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }
}

#[async_trait::async_trait]
impl KeyProvider for GdbFileKeyProvider {
    async fn get_key(&self) -> Result<String> {
        let key = tokio::fs::read_to_string(&self.path)
            .await
            .map_err(|e| anyhow!("读取密钥文件失败 {}: {e}", self.path.display()))?;
        let key = key.trim().to_string();
        if key.len() != 64 {
            return Err(anyhow!("密钥文件格式异常: len={}", key.len()));
        }
        Ok(key)
    }

    fn is_ready(&self) -> bool {
        self.path.exists()
    }
}

#[async_trait::async_trait]
impl MessageSource for Arc<DbManager> {
    async fn get_new_messages(&self) -> Result<Vec<DbMessage>> {
        self.as_ref().get_new_messages().await
    }

    fn spawn_watcher(&self) -> broadcast::Receiver<()> {
        self.spawn_wal_watcher()
    }
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct ActorPort {
    tx: mpsc::Sender<InputCommand>,
}

impl ActorPort {
    #[allow(dead_code)]
    pub fn new(tx: mpsc::Sender<InputCommand>) -> Self {
        Self { tx }
    }
}

#[async_trait::async_trait]
impl MessageSender for ActorPort {
    async fn send_message(&self, to: &str, text: &str, at: &[String]) -> Result<SendResult> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(InputCommand::SendMessage {
                to: to.to_string(),
                text: text.to_string(),
                at: at.to_vec(),
                skip_verify: false,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow!("InputEngine actor 已停止"))?;

        match reply_rx.await {
            Ok(Ok((sent, verified, message))) => Ok(SendResult {
                sent,
                verified,
                message,
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow!("actor 响应通道已关闭")),
        }
    }

    async fn send_image(&self, to: &str, image_path: &str) -> Result<SendResult> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(InputCommand::SendImage {
                to: to.to_string(),
                image_path: image_path.to_string(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow!("InputEngine actor 已停止"))?;

        match reply_rx.await {
            Ok(Ok((sent, verified, message))) => Ok(SendResult {
                sent,
                verified,
                message,
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow!("actor 响应通道已关闭")),
        }
    }
}

#[async_trait::async_trait]
impl SessionLocator for ActorPort {
    async fn chat_with(&self, who: &str) -> Result<Option<String>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(InputCommand::ChatWith {
                who: who.to_string(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow!("InputEngine actor 已停止"))?;

        match reply_rx.await {
            Ok(result) => result,
            Err(_) => Err(anyhow!("actor 响应通道已关闭")),
        }
    }

    async fn add_listen(&self, who: &str) -> Result<bool> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(InputCommand::AddListen {
                who: who.to_string(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow!("InputEngine actor 已停止"))?;

        match reply_rx.await {
            Ok(result) => result,
            Err(_) => Err(anyhow!("actor 响应通道已关闭")),
        }
    }

    async fn remove_listen(&self, who: &str) -> bool {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(InputCommand::RemoveListen {
                who: who.to_string(),
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            return false;
        }

        reply_rx.await.unwrap_or(false)
    }
}
