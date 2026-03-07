use serde::Serialize;
use tokio::sync::{broadcast, RwLock};
use tracing::info;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeState {
    Booting,
    DesktopReady,
    WeChatReady,
    LoginWaiting,
    KeyReady,
    DbReady,
    Serving,
    Degraded(String),
}

impl RuntimeState {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Booting => "Booting",
            Self::DesktopReady => "DesktopReady",
            Self::WeChatReady => "WeChatReady",
            Self::LoginWaiting => "LoginWaiting",
            Self::KeyReady => "KeyReady",
            Self::DbReady => "DbReady",
            Self::Serving => "Serving",
            Self::Degraded(_) => "Degraded",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Degraded(reason) => Some(reason.as_str()),
            _ => None,
        }
    }
}

impl std::fmt::Display for RuntimeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.reason() {
            Some(reason) => write!(f, "{}({reason})", self.name()),
            None => write!(f, "{}", self.name()),
        }
    }
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct RuntimeSnapshot {
    /// 运行时状态名称 (如 "Running", "LoginWaiting")
    pub state: String,
    /// 当前状态的可选原因说明
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl From<&RuntimeState> for RuntimeSnapshot {
    fn from(value: &RuntimeState) -> Self {
        Self {
            state: value.name().to_string(),
            reason: value.reason().map(ToOwned::to_owned),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeTransition {
    pub from: RuntimeSnapshot,
    pub to: RuntimeSnapshot,
}

pub struct RuntimeManager {
    current: RwLock<RuntimeState>,
    tx: broadcast::Sender<RuntimeTransition>,
}

impl RuntimeManager {
    pub fn new(initial: RuntimeState) -> Self {
        let (tx, _) = broadcast::channel(32);
        Self {
            current: RwLock::new(initial),
            tx,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeTransition> {
        self.tx.subscribe()
    }

    #[allow(dead_code)]
    pub async fn current(&self) -> RuntimeState {
        self.current.read().await.clone()
    }

    pub async fn snapshot(&self) -> RuntimeSnapshot {
        let state = self.current.read().await;
        RuntimeSnapshot::from(&*state)
    }

    pub async fn is_degraded(&self) -> bool {
        matches!(&*self.current.read().await, RuntimeState::Degraded(_))
    }

    pub async fn transition_to(&self, next: RuntimeState) {
        let mut current = self.current.write().await;
        if *current == next {
            return;
        }

        let previous = current.clone();
        *current = next.clone();

        info!("🧭 RuntimeState: {} -> {}", previous, next);
        let _ = self.tx.send(RuntimeTransition {
            from: RuntimeSnapshot::from(&previous),
            to: RuntimeSnapshot::from(&next),
        });
    }

    pub async fn degrade(&self, reason: impl Into<String>) {
        self.transition_to(RuntimeState::Degraded(reason.into()))
            .await;
    }
}
