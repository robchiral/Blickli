//! Slideshow playlist state supporting stepping, looping, and shuffle.

use rand::seq::SliceRandom;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub struct Playlist {
    files: Vec<PathBuf>,
    /// Index into `order` when shuffled, or directly into `files` otherwise.
    position: usize,
    /// Shuffled permutation of indices into `files`; empty when unshuffled.
    order: Vec<usize>,
    pub looping: bool,
}

impl Playlist {
    pub fn new(files: Vec<PathBuf>, start: usize) -> Self {
        let start = start.min(files.len().saturating_sub(1));
        Playlist {
            files,
            position: start,
            order: Vec::new(),
            looping: false,
        }
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn shuffled(&self) -> bool {
        !self.order.is_empty()
    }

    /// Current file's index in the sorted `files` list.
    pub fn current_index(&self) -> usize {
        if self.shuffled() {
            self.order[self.position]
        } else {
            self.position
        }
    }

    pub fn current(&self) -> Option<&PathBuf> {
        self.files.get(self.current_index())
    }

    /// File `delta` steps from the current position in play order (for prefetching).
    pub fn peek(&self, delta: i64) -> Option<&PathBuf> {
        let target = self.target_position(delta)?;
        let index = if self.shuffled() {
            self.order[target]
        } else {
            target
        };
        self.files.get(index)
    }

    /// Advance by `delta` in play order; returns `false` at boundaries when not looping.
    pub fn step(&mut self, delta: i64) -> bool {
        match self.target_position(delta) {
            Some(position) => {
                self.position = position;
                true
            }
            None => false,
        }
    }

    fn target_position(&self, delta: i64) -> Option<usize> {
        let len = self.files.len() as i64;
        if len == 0 {
            return None;
        }
        let next = self.position as i64 + delta;
        if next >= 0 && next < len {
            Some(next as usize)
        } else if self.looping {
            Some(next.rem_euclid(len) as usize)
        } else {
            None
        }
    }

    /// Jump by `delta` (Page Up/Down), clamping at ends unless looping.
    pub fn jump(&mut self, delta: i64) -> bool {
        let len = self.files.len() as i64;
        if len == 0 {
            return false;
        }
        if self.looping {
            return self.step(delta);
        }
        let target = (self.position as i64 + delta).clamp(0, len - 1) as usize;
        let moved = target != self.position;
        self.position = target;
        moved
    }

    pub fn jump_first(&mut self) {
        self.position = 0;
    }

    pub fn jump_last(&mut self) {
        self.position = self.files.len().saturating_sub(1);
    }

    /// Step in sorted order even while shuffled (option-arrow).
    pub fn step_original(&mut self, delta: i64) -> bool {
        if !self.shuffled() {
            return self.step(delta);
        }
        let len = self.files.len() as i64;
        let mut target = self.current_index() as i64 + delta;
        if self.looping {
            target = target.rem_euclid(len);
        } else if target < 0 || target >= len {
            return false;
        }
        if let Some(position) = self.order.iter().position(|&i| i == target as usize) {
            self.position = position;
            true
        } else {
            false
        }
    }

    /// Remove `paths` in one pass, keeping the play position on the next remaining item.
    pub fn remove_all(&mut self, paths: &[PathBuf]) {
        if paths.is_empty() || self.files.is_empty() {
            return;
        }
        let going: HashSet<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let mut kept = 0;
        let new_index: Vec<Option<usize>> = self
            .files
            .iter()
            .map(|file| {
                (!going.contains(file.as_path())).then(|| {
                    kept += 1;
                    kept - 1
                })
            })
            .collect();
        if kept == self.files.len() {
            return;
        }
        let gone = |file: &usize| new_index[*file].is_none();
        if self.shuffled() {
            self.position -= self.order[..self.position]
                .iter()
                .filter(|i| gone(i))
                .count();
            self.order = self.order.iter().filter_map(|&i| new_index[i]).collect();
        } else {
            self.position -= (0..self.position).filter(gone).count();
        }
        let mut index = 0;
        self.files.retain(|_| {
            index += 1;
            new_index[index - 1].is_some()
        });
        self.position = self.position.min(self.files.len().saturating_sub(1));
    }

    /// Update a path in place (e.g. when a crop converts formats).
    pub fn rename(&mut self, from: &Path, to: &Path) {
        for file in self.files.iter_mut().filter(|file| file.as_path() == from) {
            *file = to.to_path_buf();
        }
    }

    /// Toggle shuffle while keeping the current file selected.
    pub fn set_shuffled(&mut self, shuffled: bool) {
        if shuffled == self.shuffled() || self.files.is_empty() {
            return;
        }
        if shuffled {
            let current = self.current_index();
            self.shuffle_from_start();
            let at = self.order.iter().position(|&i| i == current).unwrap();
            self.order.swap(0, at);
        } else {
            self.position = self.current_index();
            self.order.clear();
        }
    }

    pub fn shuffle_from_start(&mut self) {
        self.order = (0..self.files.len()).collect();
        self.order.shuffle(&mut rand::rng());
        self.position = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playlist(n: usize) -> Playlist {
        Playlist::new((0..n).map(path).collect(), 0)
    }

    fn path(i: usize) -> PathBuf {
        PathBuf::from(format!("/f/{i}.jpg"))
    }

    fn name(p: Option<&PathBuf>) -> String {
        p.unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn steps_stop_at_the_edges_unless_looping() {
        let mut pl = playlist(3);
        assert!(pl.step(1) && pl.step(1));
        assert_eq!(name(pl.current()), "2.jpg");
        assert!(!pl.step(1));
        assert!(pl.peek(1).is_none());
        pl.looping = true;
        assert_eq!(name(pl.peek(1)), "0.jpg");
        assert!(pl.step(1));
        assert_eq!(name(pl.current()), "0.jpg");
        assert!(pl.step(-1));
        assert_eq!(name(pl.current()), "2.jpg");
    }

    #[test]
    fn page_jumps_stop_at_the_ends() {
        let mut pl = playlist(4);
        assert!(pl.jump(10));
        assert_eq!(pl.current_index(), 3);
        assert!(!pl.jump(10), "already at the end");
        assert!(pl.jump(-10));
        assert_eq!(pl.current_index(), 0);
        pl.looping = true;
        assert!(pl.jump(5));
        assert_eq!(pl.current_index(), 1);
    }

    #[test]
    fn shuffle_plays_each_file_once_and_keeps_the_current_one() {
        let mut pl = playlist(10);
        pl.step(2);
        pl.set_shuffled(true);
        assert_eq!(name(pl.current()), "2.jpg", "the current file plays first");
        let mut seen = vec![pl.current_index()];
        while pl.step(1) {
            seen.push(pl.current_index());
        }
        seen.sort_unstable();
        assert_eq!(seen, (0..10).collect::<Vec<_>>());
        let current = name(pl.current());
        pl.set_shuffled(false);
        assert_eq!(name(pl.current()), current);
    }

    #[test]
    fn step_original_follows_sorted_order_while_shuffled() {
        let mut pl = playlist(5);
        pl.step(1);
        pl.set_shuffled(true);
        assert!(pl.step_original(1));
        assert_eq!(name(pl.current()), "2.jpg");
        assert!(pl.step_original(-2));
        assert_eq!(name(pl.current()), "0.jpg");
        assert!(!pl.step_original(-1));
    }

    #[test]
    fn removing_the_current_file_moves_to_the_next() {
        let mut pl = playlist(8);
        pl.step(3);
        pl.remove_all(&[path(0), path(3), path(4)]);
        assert_eq!(name(pl.current()), "5.jpg");
        pl.remove_all(&[path(7)]);
        assert_eq!(name(pl.current()), "5.jpg");
        assert_eq!(pl.len(), 4);
        pl.jump_last();
        pl.remove_all(&[path(6)]);
        assert_eq!(name(pl.current()), "5.jpg", "past the end steps back");
        pl.remove_all(&[path(1), path(2), path(5)]);
        assert!(pl.is_empty() && pl.current().is_none());
    }

    #[test]
    fn removing_while_shuffled_keeps_the_rest_reachable() {
        let mut pl = playlist(8);
        pl.set_shuffled(true);
        pl.step(3);
        let current = name(pl.current());
        let next = name(pl.peek(1));
        let gone: Vec<PathBuf> = (0..8)
            .map(path)
            .filter(|p| name(Some(p)) != current && name(Some(p)) != next)
            .take(3)
            .collect();
        pl.remove_all(&gone);
        assert_eq!(name(pl.current()), current);
        pl.remove_all(&[PathBuf::from(format!("/f/{current}"))]);
        assert_eq!(name(pl.current()), next);
        pl.jump_first();
        let mut seen = vec![name(pl.current())];
        while pl.step(1) {
            seen.push(name(pl.current()));
        }
        assert_eq!(seen.len(), 4);
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 4);
    }

    #[test]
    fn a_renamed_file_keeps_its_place() {
        let files: Vec<PathBuf> = ["a.webp", "b.webp", "c.webp"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let mut playlist = Playlist::new(files, 1);
        playlist.rename(Path::new("b.webp"), Path::new("b.png"));
        assert_eq!(playlist.current(), Some(&PathBuf::from("b.png")));
        assert_eq!(playlist.len(), 3);
    }
}
