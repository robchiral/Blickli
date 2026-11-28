//! Batch undo/redo history for file operations and in-place edits.

use std::path::PathBuf;

const DEPTH: usize = 50;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Files moved between folders, stored as `(from, to)`.
    Moved(Vec<(PathBuf, PathBuf)>),
    /// Files moved to Trash, stored as `(original, trash_path)`.
    /// Undo moves back from Trash; redo moves to Trash again via Finder Trash.
    Trashed(Vec<(PathBuf, PathBuf)>),
    /// Newly created copies; undone by moving them to Trash.
    Copied(Vec<PathBuf>),
    /// Copies currently stashed in Trash after undoing a copy, stored as `(copy_path, trash_path)`.
    /// Redo restores them and pushes `Step::Copied` back onto the undo stack.
    TrashedCopy(Vec<(PathBuf, PathBuf)>),
    /// Files modified in place (rotation, crop, upscale, or compress).
    Edited(Vec<Edit>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    /// EXIF orientation tag rewritten without re-encoding pixels.
    Oriented { path: PathBuf, before: u8, after: u8 },
    /// File replaced in place: `current` is active, `stashed` is the previous
    /// version in Trash, and `home` is its original path. Undo and redo swap them.
    Replaced {
        current: PathBuf,
        stashed: PathBuf,
        home: PathBuf,
    },
}

impl Edit {
    /// Path of the active file on disk.
    pub fn path(&self) -> &PathBuf {
        match self {
            Edit::Oriented { path, .. } => path,
            Edit::Replaced { current, .. } => current,
        }
    }
}

impl Step {
    pub fn is_empty(&self) -> bool {
        match self {
            Step::Moved(items) | Step::Trashed(items) | Step::TrashedCopy(items) => items.is_empty(),
            Step::Copied(items) => items.is_empty(),
            Step::Edited(edits) => edits.is_empty(),
        }
    }

    /// Compute the filesystem action needed to undo or redo this batch.
    pub fn reversal(&self, undoing: bool) -> Reversal {
        match (self, undoing) {
            (Step::Moved(items), true) => Reversal::Move {
                moves: items
                    .iter()
                    .map(|(from, to)| (to.clone(), from.clone()))
                    .collect(),
                then: self.clone(),
            },
            (Step::Moved(items), false) => Reversal::Move {
                moves: items.clone(),
                then: self.clone(),
            },
            (Step::Trashed(items), true) => Reversal::Move {
                moves: items
                    .iter()
                    .map(|(orig, trash)| (trash.clone(), orig.clone()))
                    .collect(),
                then: self.clone(),
            },
            (Step::Trashed(items), false) => {
                Reversal::Trash(items.iter().map(|(orig, _)| orig.clone()).collect())
            }
            (Step::Copied(copies), true) => Reversal::Trash(copies.clone()),
            (Step::Copied(_), false) => Reversal::Move {
                moves: Vec::new(),
                then: self.clone(),
            },
            (Step::TrashedCopy(items), false) => Reversal::Move {
                moves: items
                    .iter()
                    .map(|(copy, trash)| (trash.clone(), copy.clone()))
                    .collect(),
                then: Step::Copied(items.iter().map(|(copy, _)| copy.clone()).collect()),
            },
            (Step::TrashedCopy(items), true) => {
                Reversal::Trash(items.iter().map(|(copy, _)| copy.clone()).collect())
            }
            (Step::Edited(edits), undoing) => Reversal::Edit {
                edits: edits.clone(),
                undoing,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reversal {
    /// Move each `(from, to)` pair, then push `then` onto the opposite stack.
    Move {
        moves: Vec<(PathBuf, PathBuf)>,
        then: Step,
    },
    /// Move copies to Trash; the caller records their Trash paths for redo.
    Trash(Vec<PathBuf>),
    /// Reverse or reapply in-place edits; the caller records updated Trash paths.
    Edit { edits: Vec<Edit>, undoing: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub label: String,
    pub step: Step,
}

#[derive(Default)]
pub struct History {
    done: Vec<Entry>,
    undone: Vec<Entry>,
}

impl History {
    /// Record a completed batch and clear the redo stack.
    pub fn record(&mut self, label: &str, step: Step) {
        if step.is_empty() {
            return;
        }
        self.undone.clear();
        self.done.push(Entry {
            label: label.to_owned(),
            step,
        });
        if self.done.len() > DEPTH {
            self.done.remove(0);
        }
    }

    pub fn take_undo(&mut self) -> Option<Entry> {
        self.done.pop()
    }

    pub fn take_redo(&mut self) -> Option<Entry> {
        self.undone.pop()
    }

    pub fn push_undone(&mut self, entry: Entry) {
        if !entry.step.is_empty() {
            self.undone.push(entry);
        }
    }

    pub fn push_done(&mut self, entry: Entry) {
        if !entry.step.is_empty() {
            self.done.push(entry);
        }
    }

    pub fn undo_title(&self) -> String {
        match self.done.last() {
            Some(entry) => format!("Undo {}", entry.label),
            None => "Undo".to_owned(),
        }
    }

    pub fn redo_title(&self) -> String {
        match self.undone.last() {
            Some(entry) => format!("Redo {}", entry.label),
            None => "Redo".to_owned(),
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.done.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moved(from: &str, to: &str) -> Step {
        Step::Moved(vec![(PathBuf::from(from), PathBuf::from(to))])
    }

    #[test]
    fn a_batch_goes_back_and_forward() {
        let mut history = History::default();
        assert!(!history.can_undo());
        history.record("move to trash", moved("/a/x.heic", "/trash/x.heic"));
        assert_eq!(history.undo_title(), "Undo move to trash");
        assert!(history.can_undo() && !history.can_redo());

        let entry = history.take_undo().expect("one batch to take back");
        history.push_undone(entry);
        assert!(!history.can_undo() && history.can_redo());
        assert_eq!(history.redo_title(), "Redo move to trash");

        let entry = history.take_redo().expect("one batch to redo");
        history.push_done(entry);
        assert!(history.can_undo() && !history.can_redo());
    }

    #[test]
    fn a_new_batch_ends_the_redo_timeline() {
        let mut history = History::default();
        history.record("move", moved("/a/x.heic", "/b/x.heic"));
        let entry = history.take_undo().unwrap();
        history.push_undone(entry);
        assert!(history.can_redo());
        history.record("copy", Step::Copied(vec![PathBuf::from("/b/y.heic")]));
        assert!(!history.can_redo(), "the old future is gone");
    }

    #[test]
    fn an_empty_batch_is_not_worth_remembering() {
        let mut history = History::default();
        history.record("move", Step::Moved(Vec::new()));
        history.record("copy", Step::Copied(Vec::new()));
        history.push_done(Entry {
            label: "edit".to_owned(),
            step: Step::Edited(Vec::new()),
        });
        history.push_undone(Entry {
            label: "edit".to_owned(),
            step: Step::Moved(Vec::new()),
        });
        assert!(!history.can_undo());
        assert!(!history.can_redo());
        assert_eq!(history.undo_title(), "Undo");
        assert_eq!(history.redo_title(), "Redo");
    }

    #[test]
    fn reversals_walk_moves_back_and_trash_copies() {
        let step = Step::Moved(vec![
            (PathBuf::from("/a/x.heic"), PathBuf::from("/b/x.heic")),
            (PathBuf::from("/a/y.heic"), PathBuf::from("/b/y 2.heic")),
        ]);
        let Reversal::Move { moves, then } = step.reversal(true) else {
            panic!("a move is undone by moving back");
        };
        assert_eq!(
            moves[1],
            (PathBuf::from("/b/y 2.heic"), PathBuf::from("/a/y.heic"))
        );
        assert_eq!(then, step);
        let copies = vec![PathBuf::from("/b/x.heic")];
        assert_eq!(
            Step::Copied(copies.clone()).reversal(true),
            Reversal::Trash(copies)
        );
    }

    #[test]
    fn edits_reverse_in_place() {
        let edits = vec![
            Edit::Oriented {
                path: PathBuf::from("/a/x.jpg"),
                before: 1,
                after: 6,
            },
            Edit::Replaced {
                current: PathBuf::from("/a/y.jpg"),
                stashed: PathBuf::from("/trash/y.jpg"),
                home: PathBuf::from("/a/y.jpg"),
            },
        ];
        let step = Step::Edited(edits.clone());
        assert!(!step.is_empty());
        assert!(Step::Edited(Vec::new()).is_empty());
        assert_eq!(
            step.reversal(true),
            Reversal::Edit {
                edits: edits.clone(),
                undoing: true
            }
        );
        assert_eq!(edits[1].path(), &PathBuf::from("/a/y.jpg"));
        let mut history = History::default();
        history.record("rotation", step);
        assert_eq!(history.undo_title(), "Undo rotation");
    }

    #[test]
    fn the_stack_stops_growing() {
        let mut history = History::default();
        for index in 0..DEPTH + 10 {
            history.record(&format!("move {index}"), moved("/a/x.heic", "/b/x.heic"));
        }
        let mut taken = 0;
        while history.take_undo().is_some() {
            taken += 1;
        }
        assert_eq!(taken, DEPTH);
    }
}
