//! File sort orders, single-pass sorted merges, and parallel sorting.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::path::Path;
use std::thread;
use std::time::SystemTime;

use lexical_sort::natural_lexical_cmp;

use crate::{FileDates, FileInfo};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SortOrder {
    #[default]
    Name,
    Modified,
    Size,
    Path,
    ExifDate,
    Added,
}

impl SortOrder {
    pub fn needs_file_dates(self) -> bool {
        matches!(self, Self::ExifDate | Self::Added)
    }

    pub fn needs_exif(self) -> bool {
        matches!(self, Self::ExifDate)
    }
}

/// Total ordering for files under `order`, breaking ties by natural name order.
pub fn file_info_cmp(a: &FileInfo, b: &FileInfo, order: SortOrder, descending: bool) -> Ordering {
    // Fall back to modified time when EXIF or date-added metadata is missing.
    let date = |file: &FileInfo, pick: fn(FileDates) -> Option<SystemTime>| {
        file.dates.and_then(pick).unwrap_or(file.modified)
    };
    let primary = match order {
        SortOrder::Name => file_cmp(&a.path, &b.path),
        SortOrder::Modified => a.modified.cmp(&b.modified),
        SortOrder::Size => a.size.cmp(&b.size),
        SortOrder::Path => {
            natural_lexical_cmp(&a.path.to_string_lossy(), &b.path.to_string_lossy())
        }
        SortOrder::ExifDate => date(a, |d| d.exif_date).cmp(&date(b, |d| d.exif_date)),
        SortOrder::Added => date(a, |d| d.added).cmp(&date(b, |d| d.added)),
    };
    let primary = if descending {
        primary.reverse()
    } else {
        primary
    };
    primary.then_with(|| file_cmp(&a.path, &b.path))
}

pub fn natural_str_cmp(a: &str, b: &str) -> Ordering {
    natural_lexical_cmp(a, b)
}

/// Natural filename comparison (`img2` before `img10`), breaking ties by full path.
pub(crate) fn file_cmp(a: &Path, b: &Path) -> Ordering {
    let name_a = a.file_name().unwrap_or_default().to_string_lossy();
    let name_b = b.file_name().unwrap_or_default().to_string_lossy();
    natural_lexical_cmp(&name_a, &name_b)
        .then_with(|| natural_lexical_cmp(&a.to_string_lossy(), &b.to_string_lossy()))
        .then_with(|| a.cmp(b))
}

/// Merge sorted `batch` into sorted `list` in O(n + k log n), skipping duplicates.
/// Returns the ascending final indices of inserted items.
pub fn merge_sorted_by<T>(
    list: &mut Vec<T>,
    batch: Vec<T>,
    mut cmp: impl FnMut(&T, &T) -> Ordering,
) -> Vec<usize> {
    let mut landed = Vec::with_capacity(batch.len());
    if batch.is_empty() {
        return landed;
    }
    debug_assert!(batch.is_sorted_by(|a, b| cmp(a, b).is_le()));
    let old = std::mem::take(list);
    list.reserve(old.len() + batch.len());
    let mut rest = old.into_iter();
    for item in batch {
        let Err(skip) = rest.as_slice().binary_search_by(|probe| cmp(probe, &item)) else {
            continue;
        };
        list.extend(rest.by_ref().take(skip));
        if landed
            .last()
            .is_some_and(|&at| cmp(&list[at], &item).is_eq())
        {
            continue;
        }
        landed.push(list.len());
        list.push(item);
    }
    list.extend(rest);
    landed
}

const PARALLEL_SORT_MIN: usize = 4096;

/// Parallel sort via index permutation so non-`Send` wrappers like `Rc<FileInfo>` can be sorted.
pub fn sort_files<T: Borrow<FileInfo>>(
    list: &mut Vec<T>,
    cmp: impl Fn(&FileInfo, &FileInfo) -> Ordering + Sync,
) {
    let threads = thread::available_parallelism().map_or(1, |n| n.get().min(8));
    if threads < 2 || list.len() < PARALLEL_SORT_MIN {
        list.sort_by(|a, b| cmp(a.borrow(), b.borrow()));
        return;
    }
    let order = {
        let infos: Vec<&FileInfo> = list.iter().map(T::borrow).collect();
        let mut order: Vec<usize> = (0..infos.len()).collect();
        parallel_sort(
            &mut order,
            &|&a: &usize, &b: &usize| cmp(infos[a], infos[b]),
            threads,
        );
        order
    };
    let mut slots: Vec<Option<T>> = list.drain(..).map(Some).collect();
    list.extend(order.into_iter().filter_map(|index| slots[index].take()));
}

fn parallel_sort<T: Copy + Send + Sync>(
    items: &mut [T],
    cmp: &(impl Fn(&T, &T) -> Ordering + Sync),
    threads: usize,
) {
    if threads < 2 || items.len() < PARALLEL_SORT_MIN {
        items.sort_by(cmp);
        return;
    }
    let mid = items.len() / 2;
    {
        let (left, right) = items.split_at_mut(mid);
        thread::scope(|scope| {
            scope.spawn(|| parallel_sort(left, cmp, threads / 2));
            parallel_sort(right, cmp, threads - threads / 2);
        });
    }
    let mut merged = Vec::with_capacity(items.len());
    let (mut left, mut right) = items.split_at(mid);
    while let (Some(a), Some(b)) = (left.first(), right.first()) {
        // Prefer the left element on ties to preserve stability.
        if cmp(b, a).is_lt() {
            merged.push(*b);
            right = &right[1..];
        } else {
            merged.push(*a);
            left = &left[1..];
        }
    }
    merged.extend_from_slice(left);
    merged.extend_from_slice(right);
    items.copy_from_slice(&merged);
}

/// Apply a `FilesInserted` batch to a name-sorted list in one pass when indices
/// are strictly ascending, falling back to per-item inserts otherwise.
pub fn apply_inserts<T>(
    list: &mut Vec<T>,
    inserts: Vec<(usize, FileInfo)>,
    mut wrap: impl FnMut(FileInfo) -> T,
) {
    let ascending = inserts.windows(2).all(|pair| pair[0].0 < pair[1].0);
    if inserts.len() < 2 || !ascending {
        for (index, info) in inserts {
            let at = index.min(list.len());
            list.insert(at, wrap(info));
        }
        return;
    }
    let old = std::mem::take(list);
    list.reserve(old.len() + inserts.len());
    let mut rest = old.into_iter();
    for (index, info) in inserts {
        let skip = index.saturating_sub(list.len());
        list.extend(rest.by_ref().take(skip));
        list.push(wrap(info));
    }
    list.extend(rest);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};

    fn info(name: &str, secs: u64, size: u64) -> FileInfo {
        FileInfo {
            path: PathBuf::from(format!("/x/{name}")),
            modified: UNIX_EPOCH + Duration::from_secs(secs),
            size,
            dates: None,
        }
    }

    fn names(files: &[FileInfo]) -> Vec<String> {
        files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn each_order_sorts_and_breaks_ties_by_name() {
        let mut files = vec![
            info("img10.jpg", 30, 5),
            info("img2.jpg", 10, 50),
            info("a.png", 20, 20),
        ];
        let mut sorted = |order, descending| {
            files.sort_by(|a, b| file_info_cmp(a, b, order, descending));
            names(&files)
        };
        assert_eq!(
            sorted(SortOrder::Name, false),
            ["a.png", "img2.jpg", "img10.jpg"]
        );
        assert_eq!(
            sorted(SortOrder::Name, true),
            ["img10.jpg", "img2.jpg", "a.png"]
        );
        assert_eq!(
            sorted(SortOrder::Modified, false),
            ["img2.jpg", "a.png", "img10.jpg"]
        );
        assert_eq!(
            sorted(SortOrder::Size, true),
            ["img2.jpg", "a.png", "img10.jpg"]
        );

        let mut same = vec![info("b.jpg", 5, 9), info("a.jpg", 5, 9)];
        same.sort_by(|a, b| file_info_cmp(a, b, SortOrder::Size, false));
        assert_eq!(names(&same), ["a.jpg", "b.jpg"]);
    }

    #[test]
    fn date_sorts_fall_back_to_modified() {
        let day = 86400;
        let mut a = info("a.jpg", 3 * day, 1);
        let mut b = info("b.jpg", 9 * day, 1);
        b.dates = Some(FileDates {
            exif_date: Some(UNIX_EPOCH + Duration::from_secs(day)),
            ..FileDates::default()
        });
        let first = |order, a: &FileInfo, b: &FileInfo| {
            let mut files = [a.clone(), b.clone()];
            files.sort_by(|x, y| file_info_cmp(x, y, order, false));
            files[0].path.clone()
        };
        // b's EXIF day 1 beats a's modified day 3.
        assert_eq!(first(SortOrder::ExifDate, &a, &b), b.path);
        a.dates = Some(FileDates {
            added: Some(UNIX_EPOCH + Duration::from_secs(20 * day)),
            ..FileDates::default()
        });
        // a added on day 20 loses to b's modified day 9.
        assert_eq!(first(SortOrder::Added, &a, &b), b.path);
    }

    #[test]
    fn a_merge_reports_where_each_new_item_landed() {
        let mut list = vec![10, 20, 30, 40];
        let landed = merge_sorted_by(&mut list, vec![5, 20, 25, 35, 35, 45], |a, b| a.cmp(b));
        assert_eq!(list, [5, 10, 20, 25, 30, 35, 40, 45]);
        assert_eq!(landed, [0, 3, 5, 7]);
        // Replaying those inserts in order rebuilds the same list.
        let mut replay = vec![10, 20, 30, 40];
        for &at in &landed {
            replay.insert(at, list[at]);
        }
        assert_eq!(replay, list);
    }

    #[test]
    fn inserts_apply_in_one_pass_or_one_at_a_time() {
        let mut list = vec![info("b.jpg", 0, 0), info("d.jpg", 0, 0)];
        let batch = ["a.jpg", "c.jpg", "e.jpg"].map(|name| info(name, 0, 0));
        apply_inserts(&mut list, [0, 2, 4].into_iter().zip(batch).collect(), |f| f);
        assert_eq!(names(&list), ["a.jpg", "b.jpg", "c.jpg", "d.jpg", "e.jpg"]);
        // Out of order, each index applies to the list as it stands.
        apply_inserts(
            &mut list,
            vec![(5, info("z.jpg", 0, 0)), (0, info("0.jpg", 0, 0))],
            |f| f,
        );
        assert_eq!(names(&list).first().unwrap(), "0.jpg");
        assert_eq!(names(&list).last().unwrap(), "z.jpg");
    }

    #[test]
    fn a_parallel_sort_matches_a_plain_one() {
        // Scrambled, with names that tie on everything but the folder.
        let n = 3 * PARALLEL_SORT_MIN + 17;
        let files: Vec<FileInfo> = (0..n)
            .map(|i| {
                let i = (i * 7919) % n;
                let mut file = info("", (i % 97) as u64, 0);
                file.path = PathBuf::from(format!("/x/{}/img{}.jpg", i % 2, i / 2));
                file
            })
            .collect();
        for order in [SortOrder::Name, SortOrder::Modified] {
            let cmp = |a: &FileInfo, b: &FileInfo| file_info_cmp(a, b, order, false);
            let mut plain = files.clone();
            plain.sort_by(cmp);
            let mut shared: Vec<std::rc::Rc<FileInfo>> =
                files.iter().cloned().map(std::rc::Rc::new).collect();
            sort_files(&mut shared, cmp);
            let shared: Vec<&PathBuf> = shared.iter().map(|f| &f.path).collect();
            let plain: Vec<&PathBuf> = plain.iter().map(|f| &f.path).collect();
            assert_eq!(shared, plain, "{order:?}");
        }
    }
}
