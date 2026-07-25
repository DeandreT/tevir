use std::collections::{HashSet, VecDeque};

use domain::NodeId;

const DEFAULT_REPLAY_CAPACITY: usize = 1024;

pub(crate) struct ReplayGuard {
    capacity: usize,
    order: VecDeque<(NodeId, [u8; 32])>,
    entries: HashSet<(NodeId, [u8; 32])>,
}

impl ReplayGuard {
    pub(crate) fn new() -> Self {
        Self {
            capacity: DEFAULT_REPLAY_CAPACITY,
            order: VecDeque::with_capacity(DEFAULT_REPLAY_CAPACITY),
            entries: HashSet::with_capacity(DEFAULT_REPLAY_CAPACITY),
        }
    }

    pub(crate) fn admit(&mut self, node: NodeId, nonce: [u8; 32]) -> bool {
        let entry = (node, nonce);
        if !self.entries.insert(entry.clone()) {
            return false;
        }
        self.order.push_back(entry);
        if self.order.len() > self.capacity
            && let Some(expired) = self.order.pop_front()
        {
            self.entries.remove(&expired);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use domain::NodeId;

    use super::ReplayGuard;

    #[test]
    fn rejects_a_replayed_node_nonce_pair() {
        let node =
            NodeId::new("right").unwrap_or_else(|error| panic!("invalid test node: {error}"));
        let mut guard = ReplayGuard::new();

        assert!(guard.admit(node.clone(), [3; 32]));
        assert!(!guard.admit(node.clone(), [3; 32]));
        assert!(guard.admit(node, [4; 32]));
    }
}
