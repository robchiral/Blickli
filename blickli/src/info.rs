//! Floating Info inspector panel (⌘I) and metadata formatting.

use super::*;

impl AppDelegate {
    pub(crate) fn toggle_info(&self) {
        let mtm = self.mtm();
        let window = self.ivars().info_window.get().cloned().unwrap_or_else(|| {
            let (window, label) = build_info_window(mtm);
            let _ = self.ivars().info_window.set(window.clone());
            let _ = self.ivars().info_label.set(label);
            window
        });
        if window.isVisible() {
            window.orderOut(None);
            return;
        }
        window.orderFront(None);
        self.refresh_info();
    }

    pub(crate) fn refresh_info(&self) {
        let Some(window) = self.ivars().info_window.get() else {
            return;
        };
        let Some(label) = self.ivars().info_label.get() else {
            return;
        };
        if !window.isVisible() {
            return;
        }
        label.setStringValue(&NSString::from_str(&self.info_text()));
        label.sizeToFit();
        let size = label.frame().size;
        label.setFrameOrigin(CGPoint::new(12.0, 12.0));
        if let Some(doc) = unsafe { label.superview() } {
            doc.setFrameSize(CGSize::new(size.width + 24.0, size.height + 24.0));
        }
    }

    pub(crate) fn info_text(&self) -> String {
        let paths = self.action_targets();
        match paths.as_slice() {
            [] => "No image selected.".to_owned(),
            [path] => {
                let mut rows = vec![("Name".to_owned(), file_name(path))];
                if let Some(kind) = path.extension() {
                    rows.push((
                        "Kind".to_owned(),
                        format!("{} image", kind.to_string_lossy().to_uppercase()),
                    ));
                }
                if let Ok(meta) = std::fs::metadata(path) {
                    rows.push(("Size".to_owned(), format_bytes(meta.len())));
                    if let Ok(modified) = meta.modified() {
                        rows.push(("Modified".to_owned(), date_text(modified)));
                    }
                }
                rows.extend(self.header_rows(path));
                if let Some(folder) = path.parent() {
                    rows.push(("Folder".to_owned(), folder.to_string_lossy().into_owned()));
                }
                rows_text(&rows)
            }
            many => {
                let bytes = self
                    .ivars()
                    .grid
                    .get()
                    .map_or(0, |grid| grid.selection().bytes);
                let first_parent = many[0].parent();
                let folder = if many.iter().all(|p| p.parent() == first_parent) {
                    first_parent
                        .map(|folder| folder.to_string_lossy().into_owned())
                        .unwrap_or_default()
                } else {
                    "Multiple folders".to_owned()
                };
                rows_text(&[
                    (
                        "Selected".to_owned(),
                        format!("{} images", count_text(many.len())),
                    ),
                    ("Size".to_owned(), format_bytes(bytes)),
                    ("Folder".to_owned(), folder),
                ])
            }
        }
    }

    /// Cached header metadata for `path` (invalidated on file change or path switch).
    pub(crate) fn header_rows(&self, path: &Path) -> HeaderRows {
        if let Some((cached, rows)) = self.ivars().header_rows.borrow().as_ref() {
            if cached == path {
                return rows.clone();
            }
        }
        let rows = decoder::image_info(path);
        #[cfg(any(test, feature = "e2e"))]
        self.ivars()
            .header_reads
            .set(self.ivars().header_reads.get() + 1);
        *self.ivars().header_rows.borrow_mut() = Some((path.to_path_buf(), rows.clone()));
        rows
    }
}

pub(crate) fn rows_text(rows: &[(String, String)]) -> String {
    let width = rows
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or(0);
    rows.iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(label, value)| format!("{label:<width$}   {value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub(crate) fn count_text(count: usize) -> String {
    objc2_foundation::NSNumberFormatter::localizedStringFromNumber_numberStyle(
        &objc2_foundation::NSNumber::new_usize(count),
        objc2_foundation::NSNumberFormatterStyle::DecimalStyle,
    )
    .to_string()
}

fn date_text(time: std::time::SystemTime) -> String {
    let Ok(since) = time.duration_since(std::time::UNIX_EPOCH) else {
        return String::new();
    };
    let date = objc2_foundation::NSDate::dateWithTimeIntervalSince1970(since.as_secs_f64());
    objc2_foundation::NSDateFormatter::localizedStringFromDate_dateStyle_timeStyle(
        &date,
        objc2_foundation::NSDateFormatterStyle::MediumStyle,
        objc2_foundation::NSDateFormatterStyle::ShortStyle,
    )
    .to_string()
}

/// Format an EXIF timestamp in local time to preserve camera wall-clock digits.
pub fn exif_date_text(raw: &str) -> Option<String> {
    thread_local! {
        static EXIF_PARSER: OnceCell<Retained<objc2_foundation::NSDateFormatter>> =
            const { OnceCell::new() };
    }
    let date = EXIF_PARSER.with(|cell| {
        let formatter = cell.get_or_init(|| {
            let f = objc2_foundation::NSDateFormatter::new();
            let posix = objc2_foundation::NSLocale::localeWithLocaleIdentifier(ns_string!(
                "en_US_POSIX"
            ));
            f.setLocale(Some(&posix));
            f.setDateFormat(Some(ns_string!("yyyy:MM:dd HH:mm:ss")));
            f
        });
        formatter.dateFromString(&NSString::from_str(raw))
    })?;
    Some(
        objc2_foundation::NSDateFormatter::localizedStringFromDate_dateStyle_timeStyle(
            &date,
            objc2_foundation::NSDateFormatterStyle::MediumStyle,
            objc2_foundation::NSDateFormatterStyle::ShortStyle,
        )
        .to_string(),
    )
}

fn build_info_window(mtm: MainThreadMarker) -> (Retained<NSWindow>, Retained<NSTextField>) {
    use objc2_app_kit::NSPanel;

    let frame = NSRect::new(CGPoint::new(120.0, 400.0), CGSize::new(380.0, 360.0));
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        frame,
        NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::UtilityWindow
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setFloatingPanel(true);
    let window: Retained<NSWindow> = Retained::into_super(panel);
    window.setTitle(ns_string!("Info"));
    window.setRestorable(false);
    unsafe { window.setReleasedWhenClosed(false) };
    window.setLevel(objc2_app_kit::NSFloatingWindowLevel);

    let scroll = NSScrollView::initWithFrame(
        NSScrollView::alloc(mtm),
        NSRect::new(CGPoint::new(0.0, 0.0), frame.size),
    );
    scroll.setHasVerticalScroller(true);
    scroll.setHasHorizontalScroller(true);
    scroll.setAutohidesScrollers(true);
    scroll.setDrawsBackground(false);

    // Flipped document container keeps short metadata anchored to the top-left with 12pt padding.
    let doc = prefs::FlippedView::new(
        mtm,
        NSRect::new(CGPoint::new(0.0, 0.0), scroll.contentSize()),
    );
    let label = NSTextField::labelWithString(ns_string!(""), mtm);
    label.setSelectable(true);
    label.setUsesSingleLineMode(false);
    label.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(11.0, 0.0)));
    label.setFrameOrigin(CGPoint::new(12.0, 12.0));
    doc.addSubview(&label);

    scroll.setDocumentView(Some(&doc));
    window.setContentView(Some(&scroll));
    (window, label)
}
