use crate::atspi::{AtSpi, NodeRef};

#[derive(Debug, Clone)]
pub enum NameMatch {
    Any,
    Exact(String),
    Contains(String),
    StartsWith(String),
    AnyOf(Vec<NameMatch>),
}

impl NameMatch {
    fn matches(&self, name: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(expected) => name == expected,
            Self::Contains(expected) => name.contains(expected),
            Self::StartsWith(expected) => name.starts_with(expected),
            Self::AnyOf(patterns) => patterns.iter().any(|pattern| pattern.matches(name)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NodeFingerprint {
    pub roles: Vec<String>,
    pub name_pattern: NameMatch,
    /// Ancestor roles from nearest parent outward, e.g. ["panel", "filler", "frame"].
    pub ancestor_roles: Vec<String>,
    pub sibling_index: Option<i32>,
}

impl NodeFingerprint {
    pub fn new(
        roles: impl IntoIterator<Item = impl Into<String>>,
        name_pattern: NameMatch,
    ) -> Self {
        Self {
            roles: roles.into_iter().map(Into::into).collect(),
            name_pattern,
            ancestor_roles: Vec::new(),
            sibling_index: None,
        }
    }

    async fn matches(&self, atspi: &AtSpi, node: &NodeRef) -> bool {
        let role = atspi.role(node).await;
        if !self.roles.is_empty() && !self.roles.iter().any(|item| item == &role) {
            return false;
        }

        let name = atspi.name(node).await;
        if !self.name_pattern.matches(&name) {
            return false;
        }

        if let Some(expected_index) = self.sibling_index {
            let parent = match atspi.parent(node).await {
                Some(parent) => parent,
                None => return false,
            };
            let count = atspi.child_count(&parent).await;
            let mut found_index = None;
            for idx in 0..count {
                if let Some(sibling) = atspi.child_at(&parent, idx).await {
                    if sibling.bus == node.bus && sibling.path == node.path {
                        found_index = Some(idx);
                        break;
                    }
                }
            }
            if found_index != Some(expected_index) {
                return false;
            }
        }

        if self.ancestor_roles.is_empty() {
            return true;
        }

        let mut current = atspi.parent(node).await;
        let mut collected = Vec::with_capacity(self.ancestor_roles.len());
        while let Some(parent) = current {
            collected.push(atspi.role(&parent).await);
            if collected.len() >= self.ancestor_roles.len() {
                break;
            }
            current = atspi.parent(&parent).await;
        }

        collected.len() >= self.ancestor_roles.len()
            && collected
                .windows(self.ancestor_roles.len())
                .any(|window| window == self.ancestor_roles.as_slice())
    }
}

#[derive(Debug, Clone)]
pub struct NodeHandle {
    current: Option<NodeRef>,
    fingerprint: NodeFingerprint,
    search_root: NodeRef,
}

impl NodeHandle {
    pub fn new(search_root: NodeRef, fingerprint: NodeFingerprint) -> Self {
        Self {
            current: None,
            fingerprint,
            search_root,
        }
    }

    pub fn with_current(
        search_root: NodeRef,
        fingerprint: NodeFingerprint,
        current: NodeRef,
    ) -> Self {
        Self {
            current: Some(current),
            fingerprint,
            search_root,
        }
    }

    pub fn invalidate(&mut self) {
        self.current = None;
    }

    pub fn rebind(&mut self, node: NodeRef) {
        self.current = Some(node);
    }

    pub fn set_search_root(&mut self, node: NodeRef) {
        self.search_root = node;
    }

    pub async fn is_valid(&self, atspi: &AtSpi) -> bool {
        let Some(node) = &self.current else {
            return false;
        };
        match atspi.bbox(node).await {
            Some(bbox) if bbox.w > 0 && bbox.h > 0 => self.fingerprint.matches(atspi, node).await,
            _ => false,
        }
    }

    pub async fn resolve(&mut self, atspi: &AtSpi) -> Option<NodeRef> {
        if self.is_valid(atspi).await {
            return self.current.clone();
        }

        let rebound = Self::search(atspi, &self.search_root, &self.fingerprint).await?;
        self.current = Some(rebound.clone());
        Some(rebound)
    }

    pub async fn search(
        atspi: &AtSpi,
        search_root: &NodeRef,
        fingerprint: &NodeFingerprint,
    ) -> Option<NodeRef> {
        let mut frontier = vec![search_root.clone()];
        let mut visited = 0usize;

        while !frontier.is_empty() && visited < 600 {
            let mut next = Vec::new();
            for node in frontier {
                visited += 1;
                if visited > 600 {
                    break;
                }

                if fingerprint.matches(atspi, &node).await {
                    return Some(node);
                }

                let count = atspi.child_count(&node).await;
                for idx in 0..count.min(40) {
                    if let Some(child) = atspi.child_at(&node, idx).await {
                        next.push(child);
                    }
                }
            }
            frontier = next;
        }

        None
    }
}
