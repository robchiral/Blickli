//! Viewport-driven thumbnail queue; replacing requests preserves in-flight work.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Condvar, Mutex};

pub struct ThumbnailRequest {
    pub id: u64,
    pub path: PathBuf,
    pub max_px: i32,
}

#[derive(Default)]
pub(crate) struct ThumbnailQueue {
    state: Mutex<State>,
    ready: Condvar,
}

#[derive(Default)]
struct State {
    pending: VecDeque<ThumbnailRequest>,
    wanted: HashSet<u64>,
    started: HashSet<u64>,
    closed: bool,
}

impl ThumbnailQueue {
    pub(crate) fn replace(&self, requests: Vec<ThumbnailRequest>) {
        let mut state = self.state.lock().unwrap();
        let wanted = requests
            .iter()
            .map(|request| request.id)
            .collect::<HashSet<_>>();
        state.started.retain(|id| wanted.contains(id));
        state.pending = requests
            .into_iter()
            .filter(|request| !state.started.contains(&request.id))
            .collect();
        state.wanted = wanted;
        self.ready.notify_all();
    }

    pub(crate) fn next(&self) -> Option<ThumbnailRequest> {
        let state = self.state.lock().unwrap();
        let mut state = self
            .ready
            .wait_while(state, |state| !state.closed && state.pending.is_empty())
            .unwrap();
        let request = state.pending.pop_front()?;
        state.started.insert(request.id);
        Some(request)
    }

    pub(crate) fn wants(&self, id: u64) -> bool {
        self.state.lock().unwrap().wanted.contains(&id)
    }

    pub(crate) fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        state.pending.clear();
        state.wanted.clear();
        self.ready.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_viewport_keeps_shared_work_and_replaces_the_old_queue() {
        let requests = |ids: &[u64]| {
            ids.iter()
                .map(|&id| ThumbnailRequest {
                    id,
                    path: PathBuf::from(format!("{id}.jpeg")),
                    max_px: 128,
                })
                .collect()
        };
        let queue = ThumbnailQueue::default();
        queue.replace(requests(&[1, 2, 3]));
        assert_eq!(queue.next().unwrap().id, 1);
        queue.replace(requests(&[1, 4, 5]));
        assert!(queue.wants(1));
        assert!(!queue.wants(2));
        assert_eq!(queue.next().unwrap().id, 4);
        queue.replace(requests(&[5, 6]));
        assert!(!queue.wants(4));
        assert_eq!(queue.next().unwrap().id, 5);
        assert_eq!(queue.next().unwrap().id, 6);
        queue.close();
        assert!(queue.next().is_none());
    }
}
