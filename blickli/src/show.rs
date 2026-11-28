//! Slideshow lifecycle, navigation, auto-advance timer, overlay, and crop coordination.

use super::*;

impl AppDelegate {
    /// Remove deleted paths from the active slideshow, closing it if empty.
    pub(crate) fn drop_from_slideshow(&self, paths: &[PathBuf]) {
        let (emptied, current_changed, len_changed, prefetch) = {
            let mut show = self.ivars().show.borrow_mut();
            let Some(show) = show.as_mut() else { return };
            let before_len = show.playlist.len();
            let before_current = show.playlist.current().cloned();
            show.playlist.remove_all(paths);
            for path in paths {
                show.cache.remove(path);
                show.view_states.remove(path);
                show.saving.remove(path);
            }
            let current_changed = show.playlist.current().cloned() != before_current;
            let len_changed = show.playlist.len() != before_len;
            let prefetch = if !show.playlist.is_empty() && !current_changed && len_changed {
                [1i64, -1]
                    .into_iter()
                    .filter_map(|delta| show.playlist.peek(delta).cloned())
                    .filter(|p| {
                        !decoder::is_animated_ext(p)
                            && !show.cache.contains(p)
                            && show.saving.in_flight(p) == blickli_core::Orientation::UP
                    })
                    .collect()
            } else {
                Vec::new()
            };
            (show.playlist.is_empty(), current_changed, len_changed, prefetch)
        };
        if emptied {
            self.end_slideshow();
        } else if current_changed {
            self.show_current();
        } else if len_changed {
            if let Some(engine) = self.ivars().engine.get() {
                for path in prefetch {
                    engine.prefetch_slide(path);
                }
            }
            self.update_overlay();
        }
    }

    /// Briefly display a status message over the slide even when the overlay is hidden.
    pub(crate) fn flash_overlay(&self, text: &str) {
        let overlay = {
            let mut show = self.ivars().show.borrow_mut();
            let Some(show) = show.as_mut() else { return };
            show.flash = Some(text.to_string());
            if let Some(timer) = show.flash_timer.take() {
                timer.invalidate();
            }
            show.overlay_box.clone()
        };
        overlay.setHidden(false);
        self.update_overlay();
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                2.0,
                self,
                sel!(clearFlash:),
                None,
                false,
            )
        };
        if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
            show.flash_timer = Some(timer);
        }
    }

    #[cfg(any(test, feature = "e2e"))]
    pub fn start_slideshow(&self, index: usize) {
        let files = self.ivars().grid.get().unwrap().paths();
        self.start_slideshow_files(files, index, true);
    }

    pub(crate) fn start_from_grid(&self) {
        let (files, start, keep_first) = self.ivars().grid.get().unwrap().slideshow_request();
        self.start_slideshow_files(files, start, keep_first);
    }

    fn start_slideshow_files(&self, files: Vec<PathBuf>, index: usize, keep_first: bool) {
        let mtm = self.mtm();
        if files.is_empty() {
            return;
        }
        // One slideshow at a time: a new one replaces the old.
        self.end_slideshow();
        let screen_frame = NSScreen::mainScreen(mtm)
            .map(|s| s.visibleFrame())
            .unwrap_or(NSRect::new(
                CGPoint::new(0.0, 0.0),
                CGSize::new(1440.0, 900.0),
            ));
        let window = SlideshowWindow::new(mtm, screen_frame);
        let content_frame = window.contentRectForFrameRect(window.frame());
        let view = SlideView::new(mtm, content_frame);
        window.setDelegate(Some(ProtocolObject::from_ref(self)));
        let _ = view
            .ivars()
            .delegate
            .set(objc2::Message::retain(self));

        // The info overlay: a rounded dark panel at the top left, under the
        // title bar.
        let overlay_box = objc2_app_kit::NSView::initWithFrame(
            objc2_app_kit::NSView::alloc(mtm),
            NSRect::new(
                CGPoint::new(OVERLAY_INSET, content_frame.size.height - 80.0),
                CGSize::new(200.0, 30.0),
            ),
        );
        overlay_box.setWantsLayer(true);
        if let Some(layer) = overlay_box.layer() {
            layer.setCornerRadius(9.0);
            let shade = NSColor::colorWithWhite_alpha(0.0, 0.62).CGColor();
            layer.setBackgroundColor(Some(&shade));
        }
        overlay_box.setAutoresizingMask(
            objc2_app_kit::NSAutoresizingMaskOptions::ViewMaxXMargin
                | objc2_app_kit::NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        overlay_box.setHidden(!self.ivars().overlay_visible.get());
        let overlay = NSTextField::labelWithString(ns_string!(""), mtm);
        overlay.setTextColor(Some(&NSColor::whiteColor()));
        overlay.setUsesSingleLineMode(false);
        overlay.setFont(Some(&NSFont::systemFontOfSize_weight(13.0, 0.23)));
        overlay_box.addSubview(&overlay);
        view.addSubview(&overlay_box);

        let controls = controls::Controls::new(mtm, self, &self.ivars().shortcuts.borrow());
        view.addSubview(&controls.bar);
        controls.layout(view.bounds().size);
        controls.set_enabled(self.ivars().show_controls.get());

        window.setContentView(Some(&view));
        if !snapshot::enabled() {
            window.makeKeyAndOrderFront(None);
        }
        window.makeFirstResponder(Some(&view));

        let mut playlist = Playlist::new(files, index);
        playlist.looping = self.ivars().loop_enabled.get();
        if self.ivars().shuffle_enabled.get() {
            if keep_first {
                playlist.set_shuffled(true);
            } else {
                playlist.shuffle_from_start();
            }
        }

        let initial_interval = self.ivars().default_interval.get();
        *self.ivars().show.borrow_mut() = Some(Show {
            window,
            view,
            overlay,
            overlay_box,
            controls,
            controls_timer: None,
            controls_fade_timer: None,
            crop_when_ready: false,
            upscale_when_ready: false,
            upscale_preview: None,
            playlist,
            pending_decode: None,
            cache: LruCache::new(SLIDE_CACHE_BYTES),
            view_states: HashMap::new(),
            interval: initial_interval,
            timer: None,
            flash: None,
            flash_timer: None,
            on_screen: None,
            displayed_path: None,
            last_title: String::new(),
            saving: editing::Saving::default(),
        });
        if let Some(seconds) = initial_interval {
            self.schedule_timer(seconds);
        }
        self.show_current();
        self.poke_controls();
    }

    pub fn end_slideshow(&self) {
        // Cleanup happens in windowWillClose:.
        if let Some(timer) = self
            .ivars()
            .show
            .borrow_mut()
            .as_mut()
            .and_then(|show| show.controls_timer.take())
        {
            timer.invalidate();
        }
        let window = self
            .ivars()
            .show
            .borrow()
            .as_ref()
            .map(|s| s.window.clone());
        if let Some(window) = window {
            window.close();
        }
    }

    pub fn step_slideshow(&self, delta: i64) {
        let stepped = match self.ivars().show.borrow_mut().as_mut() {
            Some(show) => show.playlist.step(delta),
            None => return,
        };
        if stepped {
            self.show_current();
        }
    }

    /// Page up and down: a jump that stops at the ends.
    pub fn jump_slideshow(&self, delta: i64) {
        let moved = match self.ivars().show.borrow_mut().as_mut() {
            Some(show) => show.playlist.jump(delta),
            None => return,
        };
        if moved {
            self.show_current();
        }
    }

    /// Option-arrow: step in sorted order while shuffled.
    pub fn step_slideshow_original(&self, delta: i64) {
        let stepped = match self.ivars().show.borrow_mut().as_mut() {
            Some(show) => show.playlist.step_original(delta),
            None => return,
        };
        if stepped {
            self.show_current();
        }
    }

    pub fn jump_slideshow_start(&self) {
        let moved = {
            let mut show = self.ivars().show.borrow_mut();
            let Some(show) = show.as_mut() else { return };
            let before = show.playlist.current_index();
            show.playlist.jump_first();
            show.playlist.current_index() != before
        };
        if moved {
            self.show_current();
        }
    }

    pub fn jump_slideshow_end(&self) {
        let moved = {
            let mut show = self.ivars().show.borrow_mut();
            let Some(show) = show.as_mut() else { return };
            let before = show.playlist.current_index();
            show.playlist.jump_last();
            show.playlist.current_index() != before
        };
        if moved {
            self.show_current();
        }
    }

    /// Space pauses auto-advance, or steps when there is none.
    pub fn space_pressed(&self) {
        let interval = self.ivars().show.borrow().as_ref().and_then(|s| s.interval);
        match interval {
            None => self.step_slideshow(1),
            Some(seconds) => {
                let paused = self
                    .ivars()
                    .show
                    .borrow()
                    .as_ref()
                    .is_some_and(|s| s.timer.is_none());
                if paused {
                    self.schedule_timer(seconds);
                } else if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
                    if let Some(timer) = show.timer.take() {
                        timer.invalidate();
                    }
                }
                self.update_overlay();
            }
        }
    }

    /// Set the pace (keys 1–9, !, @, or the menu; 0 is off). It also applies to
    /// the next show.
    pub fn set_auto_advance(&self, seconds: Option<f64>) {
        self.ivars().default_interval.set(seconds);
        self.update_auto_menu(seconds);
        let (active, editing) = {
            let mut show = self.ivars().show.borrow_mut();
            match show.as_mut() {
                Some(show) => {
                    if let Some(timer) = show.timer.take() {
                        timer.invalidate();
                    }
                    show.interval = seconds;
                    (true, show.controls.active_edit())
                }
                None => (false, false),
            }
        };
        if active {
            if !editing {
                if let Some(seconds) = seconds {
                    self.schedule_timer(seconds);
                }
            }
            self.update_overlay();
        }
        self.save_prefs();
        self.sync_prefs_controls();
    }

    pub(crate) fn schedule_timer(&self, seconds: f64) {
        let timer = unsafe {
            NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
                seconds,
                self,
                sel!(advanceSlide:),
                None,
                true,
            )
        };
        // Common modes keep it firing during event tracking.
        unsafe {
            objc2_foundation::NSRunLoop::mainRunLoop()
                .addTimer_forMode(&timer, objc2_foundation::NSRunLoopCommonModes)
        };
        if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
            show.timer = Some(timer);
        }
    }

    pub(crate) fn update_auto_menu(&self, seconds: Option<f64>) {
        let Some(menu) = self.ivars().auto_menu.get() else {
            return;
        };
        let selected_tag = prefs::tenths(seconds);
        for item in menu.itemArray() {
            item.setState(if item.tag() == selected_tag { 1 } else { 0 });
        }
    }

    /// Request a higher-resolution decode when zoom exceeds the current decoded bitmap.
    pub(crate) fn ensure_zoom_resolution(&self) {
        let Some(engine) = self.ivars().engine.get() else {
            return;
        };
        let (path, max_px) = {
            let show = self.ivars().show.borrow();
            let Some(show) = show.as_ref() else { return };
            if !show.is_current_displayed() {
                return;
            }
            let Some(path) = show.playlist.current().cloned() else {
                return;
            };
            // Skip actively animating slides; a still re-decode would stop playback.
            if show.view.is_animated() {
                return;
            }
            let Some(zoom) = show.view.state().zoom else {
                return;
            };
            let natural = show.view.natural_size();
            let backing = show.window.backingScaleFactor();
            // Cap at the Metal/Core Animation maximum texture dimension.
            const MAX_TEXTURE_EDGE: f64 = 16384.0;
            let Some(max_px) = blickli_core::zoom_decode_request(
                natural.width.max(natural.height).min(MAX_TEXTURE_EDGE) as i32,
                show.view.decoded_long_edge() as i32,
                zoom,
                backing,
            ) else {
                return;
            };
            if show
                .pending_decode
                .as_ref()
                .is_some_and(|(waiting, pending_px)| *waiting == path && *pending_px >= max_px)
            {
                return;
            }
            (path, max_px)
        };
        if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
            show.pending_decode = Some((path.clone(), max_px));
        }
        engine.request_slide_at(path, max_px);
    }

    /// The file's own pixel size, from its header.
    pub(crate) fn natural_size(path: &std::path::Path) -> CGSize {
        decoder::image_dimensions(path)
            .map(|(w, h)| CGSize::new(w as f64, h as f64))
            .unwrap_or(CGSize::new(0.0, 0.0))
    }

    pub fn slide_state_changed(&self) {
        self.save_current_slide_state();
        self.ensure_zoom_resolution();
        self.update_overlay();
    }

    /// Persist zoomed pan offset without re-evaluating zoom resolution or rebuilding overlay text.
    pub fn slide_pan_changed(&self) {
        self.save_current_slide_state();
    }

    fn save_current_slide_state(&self) {
        let mut show = self.ivars().show.borrow_mut();
        if let Some(show) = show.as_mut() {
            if !show.is_current_displayed() {
                return;
            }
            if let Some(current) = show.playlist.current().cloned() {
                let state = show.view.state();
                if state == SlideState::default() {
                    show.view_states.remove(&current);
                } else {
                    show.view_states.insert(current, state);
                }
            }
        }
    }

    pub fn toggle_exif_overlay(&self) {
        let on = !self.ivars().overlay_exif.get();
        self.ivars().overlay_exif.set(on);
        self.show_overlay_for_extra(on);
    }

    pub fn toggle_path_overlay(&self) {
        let on = !self.ivars().overlay_path.get();
        self.ivars().overlay_path.set(on);
        self.show_overlay_for_extra(on);
    }

    pub fn toggle_help_overlay(&self) {
        let on = !self.ivars().overlay_help.get();
        self.ivars().overlay_help.set(on);
        self.show_overlay_for_extra(on);
    }

    /// Turning on an extra block shows the overlay too.
    fn show_overlay_for_extra(&self, on: bool) {
        if on && !self.ivars().overlay_visible.get() {
            self.ivars().overlay_visible.set(true);
            if let Some(show) = self.ivars().show.borrow().as_ref() {
                show.overlay_box.setHidden(false);
            }
        }
        self.update_overlay();
    }

    pub fn toggle_overlay(&self) {
        let visible = !self.ivars().overlay_visible.get();
        self.ivars().overlay_visible.set(visible);
        if !visible {
            self.ivars().overlay_help.set(false);
        }
        if let Some(show) = self.ivars().show.borrow().as_ref() {
            show.overlay_box.setHidden(!visible);
        }
        self.update_overlay();
    }

    pub(crate) fn update_overlay(&self) {
        // Release the borrow before touching AppKit, which may call back in.
        let overlay_visible = self.ivars().overlay_visible.get();
        let (window, new_title, payload) = {
            let mut show = self.ivars().show.borrow_mut();
            let Some(show) = show.as_mut() else { return };
            show.controls
                .set_playing(show.timer.is_some(), &self.ivars().shortcuts.borrow());
            let name = show
                .playlist
                .current()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let new_title = if show.last_title != name {
                show.last_title = name.clone();
                Some(name.clone())
            } else {
                None
            };
            if show.flash.is_none() && !overlay_visible {
                (show.window.clone(), new_title, None)
            } else if let Some(flash) = show.flash.clone() {
                (
                    show.window.clone(),
                    new_title,
                    Some((show.overlay.clone(), flash, true, show.overlay_box.clone())),
                )
            } else {
                let mut text = format!(
                    "{}/{}  {}",
                    show.playlist.current_index() + 1,
                    show.playlist.len(),
                    name
                );
                if show.playlist.shuffled() {
                    text.push_str("  [shuffle]");
                }
                if show.playlist.looping {
                    text.push_str("  [loop]");
                }
                if let Some(seconds) = show.interval {
                    if show.timer.is_some() {
                        text.push_str(&format!("  [{seconds}s]"));
                    } else {
                        text.push_str("  [paused]");
                    }
                }
                let state = if show.is_current_displayed() {
                    show.view.state()
                } else {
                    show.playlist
                        .current()
                        .and_then(|p| show.view_states.get(p).copied())
                        .unwrap_or_default()
                };
                if state.rotation != 0 {
                    text.push_str(&format!("  [{}°]", state.rotation));
                }
                if state.flipped {
                    text.push_str("  [flipped]");
                }
                if let Some(zoom) = state.zoom {
                    text.push_str(&format!("  [{:.0}%]", zoom * 100.0));
                }
                if show.view.crop_view().is_some() {
                    text.push_str("  [cropping]");
                } else if show.view.compare_view().is_some() {
                    text.push_str("  [upscale preview]");
                } else if self.ivars().upscaling_slide.get() {
                    text.push_str("  [upscaling]");
                }
                (
                    show.window.clone(),
                    new_title,
                    Some((show.overlay.clone(), text, false, show.overlay_box.clone())),
                )
            }
        };
        if let Some(title) = new_title {
            window.setTitle(&NSString::from_str(&title));
        }
        let Some((overlay, text, is_flash, overlay_box)) = payload else {
            return;
        };
        let text = if is_flash {
            text
        } else if self.ivars().overlay_help.get() {
            self.ivars().shortcuts.borrow().slideshow_help_text()
        } else {
            self.with_overlay_extras(text)
        };
        // Blocks of rows line up in a fixed-width font; a single line reads
        // better in the system font.
        let font = if text.contains('\n') {
            NSFont::monospacedSystemFontOfSize_weight(12.0, 0.0)
        } else {
            NSFont::systemFontOfSize_weight(13.0, 0.23)
        };
        overlay.setFont(Some(&font));
        overlay.setStringValue(&NSString::from_str(&text));
        overlay.sizeToFit();
        let size = overlay.frame().size;
        overlay.setFrameOrigin(CGPoint::new(OVERLAY_PAD_X, OVERLAY_PAD_Y));
        // Keep the top-left corner below the title bar as the text grows.
        let title_h = window.frame().size.height - window.contentLayoutRect().size.height;
        let height = unsafe { overlay_box.superview() }
            .map_or(0.0, |view| view.bounds().size.height);
        let box_size = CGSize::new(size.width + 2.0 * OVERLAY_PAD_X, size.height + 2.0 * OVERLAY_PAD_Y);
        overlay_box.setFrame(NSRect::new(
            CGPoint::new(
                OVERLAY_INSET,
                height - title_h - OVERLAY_INSET + 4.0 - box_size.height,
            ),
            box_size,
        ));
    }

    fn with_overlay_extras(&self, line: String) -> String {
        let want_exif = self.ivars().overlay_exif.get();
        let want_path = self.ivars().overlay_path.get();
        if !want_exif && !want_path {
            return line;
        }
        let Some(path) = self
            .ivars()
            .show
            .borrow()
            .as_ref()
            .and_then(|show| show.playlist.current().cloned())
        else {
            return line;
        };
        let mut text = line;
        if want_path {
            text.push('\n');
            text.push_str(&path.to_string_lossy());
        }
        if want_exif {
            let rows = self.header_rows(&path);
            if rows.is_empty() {
                text.push_str("\nNo EXIF data");
            } else {
                text.push('\n');
                text.push_str(&rows_text(&rows));
            }
        }
        text
    }

    /// Display the current slide from cache or decode it, and prefetch neighbors.
    pub(crate) fn show_current(&self) {
        let engine = self.ivars().engine.get().unwrap();
        let (cropping, comparing) = {
            let show = self.ivars().show.borrow();
            match show.as_ref() {
                Some(show) => (
                    show.view.crop_view().is_some(),
                    show.view.compare_view().is_some() || self.ivars().upscaling_slide.get(),
                ),
                None => (false, false),
            }
        };
        let leaving = {
            let mut show = self.ivars().show.borrow_mut();
            let Some(show) = show.as_mut() else { return };
            let current = show.playlist.current().cloned();
            let leaving = show.on_screen.take().filter(|path| Some(path) != current.as_ref());
            show.on_screen = current;
            if leaving.is_some() {
                show.crop_when_ready = false;
                show.upscale_when_ready = false;
                show.pending_decode = None;
            }
            leaving
        };
        if let Some(path) = leaving {
            if cropping {
                self.cancel_crop();
            }
            if comparing {
                self.cancel_upscale();
            }
            self.save_slide_rotation(&path);
        }
        let (cleared_flash, cached_display, clear_anim_view) = {
            let mut show = self.ivars().show.borrow_mut();
            let Some(show) = show.as_mut() else { return };
            let had_flash = show.flash.take().is_some() | show.flash_timer.take().is_some_and(|t| {
                t.invalidate();
                true
            });
            let cleared_flash =
                had_flash.then(|| (show.overlay_box.clone(), show.controls.active_edit()));

            let mut wanted: Vec<PathBuf> = Vec::with_capacity(3);
            for delta in [0i64, 1, -1] {
                if let Some(path) = show.playlist.peek(delta) {
                    if !wanted.contains(path) {
                        wanted.push(path.clone());
                    }
                }
            }

            let mut cached_display = None;
            let mut clear_anim_view = None;
            if let Some(current) = show.playlist.current().cloned() {
                if let Some((image, natural)) = show.cache.get(&current).cloned() {
                    show.saving.redecode.remove(&current);
                    show.displayed_path = Some(current.clone());
                    let state = show.view_states.get(&current).copied();
                    cached_display = Some((show.view.clone(), image, natural, state));
                } else if show.view.is_animated()
                    && show.displayed_path.as_deref() != Some(current.as_path())
                {
                    // Stop a previous animated slide immediately instead of animating while loading.
                    show.displayed_path = None;
                    clear_anim_view = Some(show.view.clone());
                }
            }
            let current = show.playlist.current().cloned();
            for path in wanted {
                // Animated slides skip the cache and are decoded only when current.
                let animated = decoder::is_animated_ext(&path);
                let is_current = Some(&path) == current.as_ref();
                if animated && !is_current {
                    continue;
                }
                if !show.cache.contains(&path) {
                    if is_current {
                        engine.request_slide(path);
                    } else if show.saving.in_flight(&path) == blickli_core::Orientation::UP {
                        // Skip prefetching a neighbor whose rotation is still being written to disk.
                        engine.prefetch_slide(path);
                    }
                }
            }
            (cleared_flash, cached_display, clear_anim_view)
        };
        if let Some((overlay_box, active_edit)) = cleared_flash {
            overlay_box.setHidden(active_edit || !self.ivars().overlay_visible.get());
        }
        let mut restored_zoom = false;
        if let Some((view, image, natural, state)) = cached_display {
            view.show_image(image, natural);
            if let Some(state) = state {
                view.apply_state(state);
                restored_zoom = state.zoom.is_some();
            }
        } else if let Some(view) = clear_anim_view {
            view.clear_image();
        }
        if restored_zoom {
            self.ensure_zoom_resolution();
        }
        self.update_overlay();
        self.refresh_info();
    }

    /// The grid's right-click menu, acting on the slide on screen.
    pub fn slide_context_menu(&self) -> Option<Retained<NSMenu>> {
        self.ivars().grid.get().map(|grid| grid.context_menu(true))
    }

    pub(crate) fn slideshow_action_for(
        &self,
        combo: &shortcuts::KeyCombo,
    ) -> Option<shortcuts::Action> {
        self.ivars().shortcuts.borrow().slideshow_action(combo)
    }
}

const OVERLAY_INSET: f64 = 16.0;
const OVERLAY_PAD_X: f64 = 10.0;
const OVERLAY_PAD_Y: f64 = 6.0;
/// Idle delay in seconds before hiding the on-screen controls.
const CONTROLS_LINGER: f64 = 2.5;

impl AppDelegate {
    pub fn slide_view_resized(&self) {
        let Ok(show) = self.ivars().show.try_borrow() else { return };
        if let Some(show) = show.as_ref() {
            show.controls.layout(show.view.bounds().size);
        }
    }

    pub fn slide_pointer_moved(&self) {
        self.poke_controls();
    }

    pub(crate) fn poke_controls(&self) {
        if !self.ivars().show_controls.get() {
            return;
        }
        let (stale_timers, need_new_timer) = {
            let mut show = self.ivars().show.borrow_mut();
            let Some(show) = show.as_mut() else { return };
            show.controls.set_shown(true);
            let fade_timer = show.controls_fade_timer.take();
            if show.controls.active_edit() {
                (vec![fade_timer, show.controls_timer.take()], false)
            } else if let Some(timer) = show.controls_timer.as_ref().filter(|t| t.isValid()) {
                // Push back the existing idle timer in place instead of reallocating on every mouse move.
                let fire_at =
                    objc2_foundation::NSDate::dateWithTimeIntervalSinceNow(CONTROLS_LINGER);
                timer.setFireDate(&fire_at);
                (vec![fade_timer], false)
            } else {
                (vec![fade_timer, show.controls_timer.take()], true)
            }
        };
        for timer in stale_timers.into_iter().flatten() {
            timer.invalidate();
        }
        if !need_new_timer {
            return;
        }
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                CONTROLS_LINGER,
                self,
                sel!(hideControls:),
                None,
                false,
            )
        };
        if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
            show.controls_timer = Some(timer);
        }
    }

    /// Fade out controls after idle timeout unless hovered or editing.
    pub(crate) fn hide_controls(&self) {
        let (active_edit, over, pointer_inside) = {
            let mut show = self.ivars().show.borrow_mut();
            let Some(show) = show.as_mut() else { return };
            show.controls_timer = None;
            let pointer = show.view.pointer();
            let over = pointer.is_some_and(|point| show.controls.contains(point));
            (show.controls.active_edit(), over, pointer.is_some())
        };
        if active_edit {
            return;
        }
        if over {
            self.poke_controls();
            return;
        }
        let (faded, key_window) = self
            .ivars()
            .show
            .borrow()
            .as_ref()
            .map_or((false, false), |show| {
                (show.controls.set_shown(false), show.window.isKeyWindow())
            });
        if faded {
            let fade_timer = unsafe {
                NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                    0.3,
                    self,
                    sel!(controlsFaded:),
                    None,
                    false,
                )
            };
            let prev = self
                .ivars()
                .show
                .borrow_mut()
                .as_mut()
                .and_then(|show| show.controls_fade_timer.replace(fade_timer));
            if let Some(prev) = prev {
                prev.invalidate();
            }
            if key_window && pointer_inside && !e2e::enabled() && !snapshot::enabled() {
                objc2_app_kit::NSCursor::setHiddenUntilMouseMoves(true);
            }
        }
    }

    pub(crate) fn controls_faded(&self) {
        if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
            show.controls_fade_timer = None;
            show.controls.finish_fade();
        }
    }

    pub fn toggle_controls(&self) {
        let on = !self.ivars().show_controls.get();
        self.ivars().show_controls.set(on);
        if let Some(item) = self.ivars().controls_item.get() {
            item.setState(if on { 1 } else { 0 });
        }
        if !e2e::enabled() {
            NSUserDefaults::standardUserDefaults().setBool_forKey(on, ns_string!("showControls"));
        }
        let editing_bar = {
            let show = self.ivars().show.borrow();
            match show.as_ref() {
                Some(show) => {
                    show.controls.set_enabled(on || show.controls.active_edit());
                    show.controls.active_edit()
                }
                None => false,
            }
        };
        if on && !editing_bar {
            self.poke_controls();
        }
        self.sync_prefs_controls();
    }

    pub(crate) fn play_pause(&self) {
        let interval = self.ivars().show.borrow().as_ref().and_then(|s| s.interval);
        match interval {
            None => self.set_auto_advance(Some(3.0)),
            Some(_) => self.space_pressed(),
        }
    }

    pub(crate) fn slide_more(&self, sender: Option<&objc2::runtime::AnyObject>) {
        let Some(menu) = self.slide_context_menu() else { return };
        let Some(button) = sender.and_then(|s| s.downcast_ref::<objc2_app_kit::NSView>()) else {
            return;
        };
        let at = CGPoint::new(0.0, button.bounds().size.height + 4.0);
        menu.popUpMenuPositioningItem_atLocation_inView(None, at, Some(button));
    }

    pub(crate) fn zoom_slide(&self, direction: i32) {
        let compare = self
            .ivars()
            .show
            .borrow()
            .as_ref()
            .and_then(|s| s.view.compare_view());
        if let Some(compare) = compare {
            match direction {
                0 => compare.fit(),
                d => compare.zoom_step(d),
            }
            return;
        }
        let view = self.ivars().show.borrow().as_ref().map(|s| s.view.clone());
        if let Some(view) = view {
            match direction {
                0 => view.fit(),
                d => view.zoom_step(d),
            }
        }
    }

    pub fn start_crop(&self) {
        if self.ivars().transfer_busy.get()
            || self.is_upscaling_or_comparing()
            || self.is_compressing()
        {
            return;
        }
        let mtm = self.mtm();
        let (view, under) = {
            let mut show_guard = self.ivars().show.borrow_mut();
            let Some(show) = show_guard.as_mut() else {
                return;
            };
            if show.view.crop_view().is_some() {
                return;
            }
            if !show.is_current_displayed() {
                show.crop_when_ready = true;
                return;
            }
            if show.view.is_animated() {
                drop(show_guard);
                self.flash_overlay("Animated images cannot be cropped");
                return;
            }
            // Pause auto-advance and cancel idle control fade while cropping.
            if let Some(timer) = show.timer.take() {
                timer.invalidate();
            }
            if let Some(timer) = show.controls_timer.take() {
                timer.invalidate();
            }
            if let Some(timer) = show.controls_fade_timer.take() {
                timer.invalidate();
            }
            (show.view.clone(), show.overlay_box.clone())
        };
        // Inset the fitted image so handles stay clear of the title bar and control bar.
        let title_h = view
            .window()
            .map_or(28.0, |w| w.frame().size.height - w.contentLayoutRect().size.height);
        view.set_fit_insets((title_h + 16.0, 28.0, controls::CLEARANCE, 28.0));
        view.fit();
        let crop = crop::CropView::new(mtm, view.bounds(), view.image_rect());
        let _ = crop
            .ivars()
            .delegate
            .set(objc2::Message::retain(self));
        view.begin_crop(crop, &under);
        // Hide overlay so it does not obstruct the top-left crop handle.
        under.setHidden(true);
        if let Some(show) = self.ivars().show.borrow().as_ref() {
            show.controls.set_enabled(true);
            show.controls
                .set_mode(controls::BarMode::Crop, view.bounds().size);
        }
        self.crop_changed();
        self.update_overlay();
    }

    pub fn cancel_crop(&self) {
        let ended = {
            let show = self.ivars().show.borrow();
            let Some(show) = show.as_ref() else { return };
            let ended = show.view.end_crop().is_some();
            if ended {
                show.controls
                    .set_mode(controls::BarMode::Normal, show.view.bounds().size);
                show.controls.set_enabled(self.ivars().show_controls.get());
                show.overlay_box
                    .setHidden(!self.ivars().overlay_visible.get() && show.flash.is_none());
            }
            ended
        };
        if ended {
            self.poke_controls();
            self.update_overlay();
            if let Some(show) = self.ivars().show.borrow().as_ref() {
                show.window.makeFirstResponder(Some(&show.view));
            }
        }
    }

    /// Save the active crop (`replace == true` replaces the original; `false` saves a copy).
    pub fn apply_crop(&self, replace: bool) {
        let job = {
            let show = self.ivars().show.borrow();
            let Some(show) = show.as_ref() else { return };
            let (Some(crop), Some(path)) = (show.view.crop_view(), show.playlist.current().cloned())
            else {
                return;
            };
            // Subtract any queued rotation that will complete before this crop job runs.
            let orient = Self::pending_view_orientation(show, &path);
            (path, orient, crop.fractions())
        };
        self.cancel_crop();
        let (path, view, fractions) = job;
        self.queue_crop(path, view, fractions, replace);
    }

    pub fn crop_changed(&self) {
        let Ok(show) = self.ivars().show.try_borrow() else { return };
        let Some(show) = show.as_ref() else { return };
        let Some(crop) = show.view.crop_view() else { return };
        let natural = show.view.natural_size();
        let (w, h) = if show.view.state().rotation % 180 == 90 {
            (natural.height, natural.width)
        } else {
            (natural.width, natural.height)
        };
        let f = crop.fractions();
        let text = format!(
            "{} × {}",
            (f.width * w).round().max(1.0),
            (f.height * h).round().max(1.0)
        );
        show.controls
            .size_label
            .setStringValue(&NSString::from_str(&text));
    }

    pub(crate) fn crop_aspect_chosen(&self) {
        let show = self.ivars().show.borrow();
        let Some(show) = show.as_ref() else { return };
        let Some(crop) = show.view.crop_view() else { return };
        let index = show.controls.aspect.indexOfSelectedItem().max(0) as usize;
        let raw = controls::ASPECTS.get(index).and_then(|(_, a)| *a);
        let match_orientation = raw != Some(0.0);
        let aspect = raw.map(|a| {
            if a == 0.0 {
                let image = crop.image_rect();
                image.size.width / image.size.height
            } else {
                a
            }
        });
        crop.set_aspect(aspect, match_orientation);
    }

    /// Enter interactive upscaling on the current slide (starts inference immediately).
    pub fn start_upscale_preview(&self) {
        if self.is_busy() {
            return;
        }
        // Check slide readiness and pause auto-advance before opening any modal model chooser.
        let ready = {
            let mut show_guard = self.ivars().show.borrow_mut();
            let Some(show) = show_guard.as_mut() else {
                return;
            };
            if show.view.crop_view().is_some()
                || show.view.compare_view().is_some()
                || self.ivars().upscaling_slide.get()
            {
                return;
            }
            if !show.is_current_displayed() {
                show.upscale_when_ready = true;
                return;
            }
            if show.view.is_animated() {
                drop(show_guard);
                self.flash_overlay("Animated images cannot be upscaled");
                return;
            }
            let Some(path) = show.playlist.current().cloned() else {
                return;
            };
            if let Some(timer) = show.timer.take() {
                timer.invalidate();
            }
            if let Some(timer) = show.controls_timer.take() {
                timer.invalidate();
            }
            if let Some(timer) = show.controls_fade_timer.take() {
                timer.invalidate();
            }
            path
        };
        let Some(model_path) = self.ensure_model_path() else {
            self.update_overlay();
            return;
        };
        // Flush any pending rotation so `upscale_worker` never races `edit_worker` on `ready`.
        self.save_slide_rotation_now(&ready);
        let (path, view_orientation, slide_view, overlay_box) = {
            let show_guard = self.ivars().show.borrow();
            let Some(show) = show_guard.as_ref() else {
                return;
            };
            if show.playlist.current() != Some(&ready)
                || show.view.crop_view().is_some()
                || show.view.compare_view().is_some()
            {
                return;
            }
            let orient = Self::pending_view_orientation(show, &ready);
            (
                ready,
                orient,
                show.view.clone(),
                show.overlay_box.clone(),
            )
        };

        overlay_box.setHidden(true);
        if let Some(show) = self.ivars().show.borrow().as_ref() {
            show.controls
                .upscale_status
                .setStringValue(ns_string!("Preparing model…"));
            show.controls.set_enabled(true);
            show.controls
                .set_mode(controls::BarMode::Upscaling, slide_view.bounds().size);
        }
        self.queue_upscale_preview(path, view_orientation, model_path);
        self.update_overlay();
    }

    /// Update the live HUD progress string while a slide upscale is running.
    pub(crate) fn upscale_preview_progress(&self, job_id: u64, progress: crate::upscale::Progress) {
        if self.ivars().upscale_job_id.get() != job_id || !self.ivars().upscaling_slide.get() {
            return;
        }
        let text = match progress {
            crate::upscale::Progress::PreparingModel => "Preparing model…".to_owned(),
            crate::upscale::Progress::Tile { done, total } => {
                if total <= 1 {
                    "Upscaling…".to_owned()
                } else {
                    format!("Upscaling… {done}/{total}")
                }
            }
        };
        if let Some(show) = self.ivars().show.borrow().as_ref() {
            show.controls
                .upscale_status
                .setStringValue(&NSString::from_str(&text));
        }
    }

    /// Mount `CompareView` once interactive upscaling finishes.
    pub(crate) fn upscale_preview_ready(
        &self,
        job_id: u64,
        path: PathBuf,
        result: Result<crate::upscale::UpscaleResult, String>,
    ) {
        if self.ivars().upscale_job_id.get() != job_id {
            return;
        }
        self.ivars().upscaling.set(false);
        self.ivars().upscaling_slide.set(false);
        *self.ivars().upscale_cancel.borrow_mut() = None;
        self.update_status();

        let result = match result {
            Ok(result) => result,
            Err(why) => {
                self.exit_upscale_ui();
                if why != "cancelled" {
                    let message = format!("Could not upscale {}: {why}", file_name(&path));
                    self.show_status_message(&message);
                    self.flash_overlay(&message);
                }
                return;
            }
        };

        let mtm = self.mtm();
        let default_scale = self.ivars().upscale_scale.get();
        let mut preview = UpscalePreview {
            path,
            original: result.original.clone(),
            upscaled_native: result.upscaled_native,
            native_scale: result.native_scale,
            upscaled_alt: None,
            selected_scale: default_scale,
        };
        let initial_img = preview.image_for(default_scale);

        let (slide_view, overlay_box, window) = {
            let mut show = self.ivars().show.borrow_mut();
            let Some(show) = show.as_mut() else { return };
            show.upscale_preview = Some(preview);
            (
                show.view.clone(),
                show.overlay_box.clone(),
                show.window.clone(),
            )
        };

        let title_h = window.frame().size.height - window.contentLayoutRect().size.height;
        let insets = (title_h + 10.0, 12.0, controls::CLEARANCE, 12.0);
        let compare = compare::CompareView::new(
            mtm,
            slide_view.bounds(),
            result.original,
            initial_img,
            default_scale,
            insets,
        );
        let _ = compare
            .ivars()
            .delegate
            .set(objc2::Message::retain(self));
        slide_view.begin_compare(compare.clone(), &overlay_box);
        overlay_box.setHidden(true);
        if let Some(show) = self.ivars().show.borrow().as_ref() {
            show.controls.set_compare_mode(compare::CompareMode::Split);
            show.controls.set_upscale_scale(default_scale);
            show.controls.set_enabled(true);
            show.controls
                .set_mode(controls::BarMode::UpscalePreview, slide_view.bounds().size);
        }
        self.update_upscale_dims_label();
        window.makeFirstResponder(Some(&compare));
        self.update_overlay();
    }

    pub(crate) fn set_compare_mode(&self, mode: compare::CompareMode) {
        let show = self.ivars().show.borrow();
        let Some(show) = show.as_ref() else { return };
        if let Some(compare) = show.view.compare_view() {
            compare.set_mode(mode);
        }
        show.controls.set_compare_mode(mode);
    }

    pub(crate) fn upscale_compare_mode_chosen(&self) {
        let mode = {
            let show = self.ivars().show.borrow();
            let Some(show) = show.as_ref() else { return };
            if show.controls.compare_mode.selectedSegment() == 1 {
                compare::CompareMode::SideBySide
            } else {
                compare::CompareMode::Split
            }
        };
        self.set_compare_mode(mode);
    }

    pub(crate) fn upscale_preview_scale_chosen(&self) {
        let scale = {
            let show = self.ivars().show.borrow();
            let Some(show) = show.as_ref() else { return };
            if show.controls.upscale_scale.selectedSegment() == 0 {
                crate::upscale::UpscaleScale::X2
            } else {
                crate::upscale::UpscaleScale::X4
            }
        };
        self.set_upscale_preview_scale(scale);
    }

    /// Switch between 2× and 4× inside Upscale preview without re-running Core AI.
    pub(crate) fn set_upscale_preview_scale(&self, scale: crate::upscale::UpscaleScale) {
        let swapped = {
            let mut show = self.ivars().show.borrow_mut();
            let Some(show) = show.as_mut() else { return };
            let Some(preview) = show.upscale_preview.as_mut() else {
                return;
            };
            preview.selected_scale = scale;
            let image = preview.image_for(scale);
            show.controls.set_upscale_scale(scale);
            show.view.compare_view().map(|c| (c, image))
        };
        if let Some((compare, image)) = swapped {
            compare.set_upscaled(image, scale);
        }
        self.update_upscale_dims_label();
    }

    fn update_upscale_dims_label(&self) {
        let show = self.ivars().show.borrow();
        let Some(show) = show.as_ref() else { return };
        let Some(preview) = show.upscale_preview.as_ref() else {
            return;
        };
        let ow = CGImage::width(Some(&preview.original));
        let oh = CGImage::height(Some(&preview.original));
        let target_img = preview.current_image();
        let nw = CGImage::width(Some(target_img));
        let nh = CGImage::height(Some(target_img));
        let text = format!("{ow}×{oh} → {nw}×{nh}");
        show.controls
            .upscale_dims
            .setStringValue(&NSString::from_str(&text));
    }

    pub fn cancel_upscale(&self) {
        // Direct batch/single upscales keep their job id and busy flag until the worker
        // replies so any already-saved files are still recorded in Undo history.
        if self.ivars().upscaling.get() && !self.ivars().upscaling_slide.get() {
            if let Some(cancel) = self.ivars().upscale_cancel.borrow().as_ref() {
                cancel.store(true, Ordering::Release);
            }
            self.show_status_message("Cancelling upscale…");
            return;
        }
        if let Some(cancel) = self.ivars().upscale_cancel.borrow_mut().take() {
            cancel.store(true, Ordering::Release);
        }
        self.ivars()
            .upscale_job_id
            .set(self.ivars().upscale_job_id.get().wrapping_add(1));
        let was_slide = self.ivars().upscaling_slide.replace(false);
        self.ivars().upscaling.set(false);
        let had_preview = self.exit_upscale_ui();
        self.update_status();
        if was_slide && !had_preview {
            self.flash_overlay("Upscale cancelled");
        }
    }

    /// Tear down an in-flight slide upscale when closing the slideshow window without
    /// scheduling fresh control or flash timers on the closing window.
    pub(crate) fn cancel_slide_upscale_for_close(&self) {
        if !self.ivars().upscaling_slide.get() {
            return;
        }
        if let Some(cancel) = self.ivars().upscale_cancel.borrow_mut().take() {
            cancel.store(true, Ordering::Release);
        }
        self.ivars()
            .upscale_job_id
            .set(self.ivars().upscale_job_id.get().wrapping_add(1));
        self.ivars().upscaling_slide.set(false);
        self.ivars().upscaling.set(false);
        self.update_status();
    }

    fn exit_upscale_ui(&self) -> bool {
        let had = {
            let mut show = self.ivars().show.borrow_mut();
            let Some(show) = show.as_mut() else {
                return false;
            };
            let had_compare = show.view.end_compare().is_some();
            let had_preview = show.upscale_preview.take().is_some();
            let was_upscale_bar = show.controls.active_edit() && !show.controls.cropping();
            if had_compare || had_preview || was_upscale_bar {
                show.controls
                    .set_mode(controls::BarMode::Normal, show.view.bounds().size);
                show.controls.set_enabled(self.ivars().show_controls.get());
                show.overlay_box
                    .setHidden(!self.ivars().overlay_visible.get() && show.flash.is_none());
            }
            had_compare || had_preview
        };
        self.poke_controls();
        self.update_overlay();
        if let Some(show) = self.ivars().show.borrow().as_ref() {
            show.window.makeFirstResponder(Some(&show.view));
        }
        had
    }

    /// Save the previewed upscaled image (`replace == true` replaces original; `false` saves copy).
    pub fn apply_upscale(&self, replace: bool) {
        let save = {
            let mut show = self.ivars().show.borrow_mut();
            let Some(show) = show.as_mut() else { return };
            let Some(mut preview) = show.upscale_preview.take() else {
                return;
            };
            let image = preview.image_for(preview.selected_scale);
            (preview.path, image)
        };
        self.exit_upscale_ui();
        let (path, image) = save;
        self.queue_save_upscaled(path, image, replace);
    }
}

impl UpscalePreview {
    fn image_for(&mut self, scale: crate::upscale::UpscaleScale) -> Img {
        if scale.factor() == self.native_scale {
            return self.upscaled_native.clone();
        }
        if let Some((cached_scale, img)) = &self.upscaled_alt {
            if *cached_scale == scale {
                return img.clone();
            }
        }
        match crate::upscale::image_for_scale(&self.upscaled_native, self.native_scale, scale) {
            Ok(img) => {
                self.upscaled_alt = Some((scale, img.clone()));
                img
            }
            Err(_) => self.upscaled_native.clone(),
        }
    }

    fn current_image(&self) -> &Img {
        if self.selected_scale.factor() == self.native_scale {
            return &self.upscaled_native;
        }
        self.upscaled_alt
            .as_ref()
            .filter(|(scale, _)| *scale == self.selected_scale)
            .map(|(_, img)| img)
            .unwrap_or(&self.upscaled_native)
    }
}
