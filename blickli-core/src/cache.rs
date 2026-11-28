//! Byte-budgeted O(1) LRU cache. An oversized entry is kept until the next insert.

use std::collections::HashMap;
use std::hash::Hash;

pub struct LruCache<K, V> {
    budget: usize,
    total: usize,
    index: HashMap<K, usize>,
    nodes: Vec<Option<Node<K, V>>>,
    free: Vec<usize>,
    /// Most recently used slot.
    head: Option<usize>,
    /// Least recently used slot.
    tail: Option<usize>,
}

struct Node<K, V> {
    key: K,
    value: V,
    cost: usize,
    prev: Option<usize>,
    next: Option<usize>,
}

impl<K: Eq + Hash + Clone, V> LruCache<K, V> {
    pub fn new(budget: usize) -> Self {
        LruCache {
            budget,
            total: 0,
            index: HashMap::new(),
            nodes: Vec::new(),
            free: Vec::new(),
            head: None,
            tail: None,
        }
    }

    pub fn get(&mut self, key: &K) -> Option<&V> {
        let &slot = self.index.get(key)?;
        self.move_to_front(slot);
        self.nodes[slot].as_ref().map(|n| &n.value)
    }

    /// Check membership without updating recency.
    pub fn contains(&self, key: &K) -> bool {
        self.index.contains_key(key)
    }

    /// Insert as most recently used, evicting older entries past the budget.
    /// Returns evicted keys (never the newly inserted key).
    pub fn insert(&mut self, key: K, value: V, cost: usize) -> Vec<K> {
        self.remove(&key);
        self.total += cost;
        let node = Node {
            key: key.clone(),
            value,
            cost,
            prev: None,
            next: self.head,
        };
        let slot = match self.free.pop() {
            Some(slot) => {
                self.nodes[slot] = Some(node);
                slot
            }
            None => {
                let slot = self.nodes.len();
                self.nodes.push(Some(node));
                slot
            }
        };
        if let Some(old_head) = self.head {
            if let Some(head_node) = self.nodes[old_head].as_mut() {
                head_node.prev = Some(slot);
            }
        } else {
            self.tail = Some(slot);
        }
        self.head = Some(slot);
        self.index.insert(key, slot);

        let mut evicted = Vec::new();
        while self.total > self.budget && self.index.len() > 1 {
            let Some(lru) = self.tail else { break };
            if Some(lru) == self.head {
                break;
            }
            if let Some(node) = self.unlink_and_take(lru) {
                self.index.remove(&node.key);
                self.total -= node.cost;
                evicted.push(node.key);
            }
        }
        evicted
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        let slot = self.index.remove(key)?;
        let node = self.unlink_and_take(slot)?;
        self.total -= node.cost;
        Some(node.value)
    }

    pub fn clear(&mut self) {
        self.index.clear();
        self.nodes.clear();
        self.free.clear();
        self.head = None;
        self.tail = None;
        self.total = 0;
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    fn move_to_front(&mut self, slot: usize) {
        if self.head == Some(slot) {
            return;
        }
        let (prev, next) = match self.nodes[slot].as_ref() {
            Some(node) => (node.prev, node.next),
            None => return,
        };
        if let Some(prev) = prev {
            if let Some(node) = self.nodes[prev].as_mut() {
                node.next = next;
            }
        }
        if let Some(next) = next {
            if let Some(node) = self.nodes[next].as_mut() {
                node.prev = prev;
            }
        } else {
            self.tail = prev;
        }
        let old_head = self.head;
        if let Some(head) = old_head {
            if let Some(node) = self.nodes[head].as_mut() {
                node.prev = Some(slot);
            }
        }
        if let Some(node) = self.nodes[slot].as_mut() {
            node.prev = None;
            node.next = old_head;
        }
        self.head = Some(slot);
    }

    fn unlink_and_take(&mut self, slot: usize) -> Option<Node<K, V>> {
        let node = self.nodes[slot].take()?;
        self.free.push(slot);
        match node.prev {
            Some(prev) => {
                if let Some(prev_node) = self.nodes[prev].as_mut() {
                    prev_node.next = node.next;
                }
            }
            None => self.head = node.next,
        }
        match node.next {
            Some(next) => {
                if let Some(next_node) = self.nodes[next].as_mut() {
                    next_node.prev = node.prev;
                }
            }
            None => self.tail = node.prev,
        }
        Some(node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used_past_budget() {
        let mut cache: LruCache<&str, u32> = LruCache::new(100);
        assert!(cache.insert("a", 1, 40).is_empty());
        assert!(cache.insert("b", 2, 40).is_empty());
        // Touch a so b is now the oldest.
        assert_eq!(cache.get(&"a"), Some(&1));
        let evicted = cache.insert("c", 3, 40);
        assert_eq!(evicted, vec!["b"]);
        assert!(cache.contains(&"a"));
        assert!(cache.contains(&"c"));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn oversized_entry_stays_until_replaced() {
        let mut cache: LruCache<&str, u32> = LruCache::new(10);
        assert!(cache.insert("big", 1, 50).is_empty());
        assert!(cache.contains(&"big"));
        // The next insert pushes the oversized one out.
        let evicted = cache.insert("next", 2, 50);
        assert_eq!(evicted, vec!["big"]);
        assert!(cache.contains(&"next"));
    }

    #[test]
    fn remove_and_reinsert_keep_totals_right() {
        let mut cache: LruCache<&str, u32> = LruCache::new(100);
        cache.insert("a", 1, 60);
        cache.insert("a", 2, 30); // replace: total is 30, not 90
        assert!(cache.insert("b", 3, 60).is_empty());
        assert_eq!(cache.remove(&"a"), Some(2));
        assert!(cache.insert("c", 4, 40).is_empty());
        assert_eq!(cache.len(), 2);
        cache.clear();
        assert!(cache.is_empty());
    }
}
