use super::*;
use gtk::{cairo, gio};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

enum ExportEvent {
    Progress(f64),
    MouseSession(clicks::Session),
    Finished(Result<(PathBuf, Option<String>), String>),
}

struct Workspace {
    window: gtk::ApplicationWindow,
    primary: gtk::Button,
    status: gtk::Label,
    detail: gtk::Label,
    badge: gtk::Label,
    preview_label: gtk::Label,
    progress: gtk::ProgressBar,
    settings: gtk::Box,
    system_audio: gtk::Switch,
    microphone: gtk::Switch,
    follow: gtk::Switch,
    mouse_note: gtk::Label,
    mouse_session: RefCell<Option<clicks::Session>>,
    mouse_authorizing: Cell<bool>,
    preview: gtk::DrawingArea,
    preview_button: gtk::Button,
    refresh_button: gtk::Button,
    refreshing_preview: Cell<bool>,
    preview_aspect: gtk::AspectFrame,
    preview_scene: RefCell<Option<Preview>>,
    preview_started: Cell<Instant>,
    stack: gtk::Stack,
    video: gtk::Video,
    open: gtk::Button,
    style: Cell<Style>,
    countdown: Cell<u8>,
    countdown_generation: Cell<u64>,
    rendering: Cell<bool>,
    recording: RefCell<Option<ActiveRecording>>,
    latest: RefCell<Option<PathBuf>>,
    tx: mpsc::Sender<ExportEvent>,
}

fn label(text: &str, class: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes([class])
        .build()
}

fn column(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Vertical, spacing)
}
fn row(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Horizontal, spacing)
}

fn toggle_row(title: &str, copy: &str, active: bool) -> (gtk::Box, gtk::Switch) {
    let row = row(18);
    let text = column(4);
    text.set_hexpand(true);
    text.append(&label(title, "setting-title"));
    text.append(&label(copy, "muted"));
    let toggle = gtk::Switch::builder()
        .active(active)
        .valign(gtk::Align::Center)
        .build();
    toggle.update_property(&[gtk::accessible::Property::Label(title)]);
    row.append(&text);
    row.append(&toggle);
    (row, toggle)
}

pub(super) fn build_ui(app: &gtk::Application) {
    let preview_scene = Preview::capture().ok();
    let provider = gtk::CssProvider::new();
    provider.load_from_data(include_str!("style.css"));
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().unwrap(),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Screenema")
        .default_width(1120)
        .default_height(860)
        .build();
    window.add_css_class("screenema");
    let header = gtk::HeaderBar::new();
    let brand = row(10);
    brand.append(&label("◉", "brand-icon"));
    brand.append(&label("Screenema", "brand"));
    header.pack_start(&brand);
    header.set_title_widget(Some(&label("YOUR SCREEN. BEAUTIFULLY FRAMED.", "eyebrow")));
    let folder = gtk::Button::with_label("Recordings ↗");
    folder.add_css_class("quiet");
    header.pack_end(&folder);
    window.set_titlebar(Some(&header));

    let root = column(24);
    for set in [gtk::Widget::set_margin_start, gtk::Widget::set_margin_end] {
        set(root.upcast_ref(), 30);
    }
    root.set_margin_top(24);
    root.set_margin_bottom(24);
    let intro = row(12);
    let titles = column(6);
    titles.set_hexpand(true);
    titles.append(&label("Give your screen the spotlight.", "title"));
    titles.append(&label(
        "Turn a quick walkthrough into something worth sharing.",
        "subtitle",
    ));
    let badge = label("●  READY", "badge");
    badge.set_valign(gtk::Align::Center);
    intro.append(&titles);
    intro.append(&badge);
    root.append(&intro);

    let body = row(24);
    body.set_vexpand(true);
    let left = column(12);
    left.set_hexpand(true);
    left.set_valign(gtk::Align::Start);
    let preview_heading = row(12);
    let heading = label("YOUR RECORDING", "eyebrow");
    heading.set_hexpand(true);
    preview_heading.append(&heading);
    let preview_label = label("Your display · snapshot", "muted");
    preview_heading.append(&preview_label);
    left.append(&preview_heading);
    let preview = gtk::DrawingArea::builder()
        .content_width(320)
        .content_height(180)
        .hexpand(true)
        .vexpand(true)
        .build();
    preview.update_property(&[gtk::accessible::Property::Label(
        "Actual display snapshot rendered with the export camera",
    )]);
    let video = gtk::Video::new();
    video.set_autoplay(false);
    video.set_hexpand(true);
    video.set_vexpand(true);
    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    stack.add_named(&preview, Some("preview"));
    stack.add_named(&video, Some("video"));
    stack.set_visible_child_name("preview");
    let ratio = preview_scene
        .as_ref()
        .map(|p| p.geometry.width as f32 / p.geometry.height as f32)
        .unwrap_or(16.0 / 9.0);
    let aspect = gtk::AspectFrame::new(0.5, 0.0, ratio, false);
    aspect.set_child(Some(&stack));
    left.append(&aspect);
    let caption = row(16);
    let preview_note = label("Native pixels   ·   One scene   ·   60 fps", "muted");
    preview_note.set_hexpand(true);
    caption.append(&preview_note);
    caption.append(&label("60 FPS", "pill"));
    left.append(&caption);
    let tip = label(
        "Real display snapshot. The same camera and framing as your export.",
        "preview-note",
    );
    tip.set_margin_top(6);
    tip.set_wrap(true);
    left.append(&tip);
    let replay = gtk::Button::with_label("↻  Preview camera move");
    replay.add_css_class("quiet");
    replay.set_halign(gtk::Align::Start);
    let preview_tools = row(8);
    let refresh = gtk::Button::with_label("Refresh display");
    refresh.add_css_class("quiet");
    preview_tools.append(&replay);
    preview_tools.append(&refresh);
    left.append(&preview_tools);
    body.append(&left);

    let settings = column(22);
    settings.add_css_class("settings");
    settings.set_size_request(290, -1);
    settings.set_valign(gtk::Align::Start);
    settings.append(&label("RECORDING SETUP", "eyebrow"));
    let monitor = focused_monitor()
        .map(|m| m.name)
        .unwrap_or_else(|_| "Hyprland display".into());
    let display = column(6);
    display.add_css_class("display-card");
    display.append(&label("▣   Focused display", "setting-title"));
    display.append(&label(&format!("{monitor}  ·  Full resolution"), "muted"));
    settings.append(&display);
    let (system_row, system_audio) =
        toggle_row("System audio", "Include the sound from your apps", true);
    let (mic_row, microphone) = toggle_row("Microphone", "Add your voice to the story", false);
    settings.append(&system_row);
    settings.append(&mic_row);
    settings.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let background_section = column(10);
    background_section.append(&label("Backdrop", "setting-title"));
    let swatches = gtk::Grid::new();
    swatches.set_column_homogeneous(true);
    swatches.set_column_spacing(8);
    swatches.set_row_spacing(8);
    let mut backgrounds = Vec::new();
    for (index, (name, _, _)) in BACKGROUNDS.iter().enumerate() {
        let button = gtk::ToggleButton::with_label(name);
        button.add_css_class("swatch");
        button.add_css_class(&name.to_lowercase());
        button.set_tooltip_text(Some(&format!("Use the {name} backdrop")));
        if let Some(first) = backgrounds.first() {
            button.set_group(Some(first));
        }
        button.set_active(index == 0);
        swatches.attach(&button, (index % 3) as i32, (index / 3) as i32, 1, 1);
        backgrounds.push(button);
    }
    background_section.append(&swatches);
    settings.append(&background_section);
    let (motion_row, follow) = toggle_row(
        "Auto camera",
        "Smooth follow. Steady on small moves.",
        false,
    );
    settings.append(&motion_row);
    let mouse_note = label("Enable auto-camera to allow click detection.", "muted");
    mouse_note.set_wrap(true);
    mouse_note.set_max_width_chars(34);
    settings.append(&mouse_note);
    let format = label(
        "Native pixels + framing  /  60 fps\nOne finished MP4. No duplicate original.",
        "format-note",
    );
    settings.append(&format);
    body.append(&settings);
    root.append(&body);

    let bottom = column(12);
    bottom.add_css_class("transport");
    let transport = row(20);
    let status_column = column(6);
    status_column.set_hexpand(true);
    let status = label("Your next great demo starts here.", "status");
    let detail = label(
        "3-second countdown. Finished videos saved to Videos/OmaScreenema.",
        "muted",
    );
    detail.set_wrap(true);
    detail.set_max_width_chars(70);
    detail.set_selectable(true);
    status_column.append(&status);
    status_column.append(&detail);
    transport.append(&status_column);
    let open = gtk::Button::with_label("Open video ↗");
    open.add_css_class("quiet");
    open.set_visible(false);
    transport.append(&open);
    let primary = gtk::Button::with_label("●   Start recording");
    primary.add_css_class("primary");
    primary.set_valign(gtk::Align::Center);
    transport.append(&primary);
    bottom.append(&transport);
    let progress = gtk::ProgressBar::new();
    progress.set_visible(false);
    bottom.append(&progress);
    root.append(&bottom);
    let footer = row(12);
    let privacy = label(
        "LOCAL BY DESIGN. YOUR RECORDINGS STAY ON YOUR MACHINE.",
        "footer",
    );
    privacy.set_hexpand(true);
    footer.append(&privacy);
    footer.append(&label("Ctrl + Shift + R   Record / Stop", "muted"));
    root.append(&footer);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&root)
        .build();
    window.set_child(Some(&scroll));

    let (tx, rx) = mpsc::channel();
    let ui = Rc::new(Workspace {
        window,
        primary,
        status,
        detail,
        badge,
        preview_label,
        progress,
        settings,
        system_audio,
        microphone,
        follow,
        mouse_note,
        mouse_session: RefCell::new(None),
        mouse_authorizing: Cell::new(false),
        preview,
        preview_button: replay.clone(),
        refresh_button: refresh.clone(),
        refreshing_preview: Cell::new(false),
        preview_aspect: aspect,
        preview_scene: RefCell::new(preview_scene),
        preview_started: Cell::new(Instant::now()),
        stack,
        video,
        open,
        style: Cell::new(Style::default()),
        countdown: Cell::new(0),
        countdown_generation: Cell::new(0),
        rendering: Cell::new(false),
        recording: RefCell::new(None),
        latest: RefCell::new(None),
        tx,
    });
    for (index, button) in backgrounds.into_iter().enumerate() {
        let weak = Rc::downgrade(&ui);
        button.connect_toggled(move |button| {
            if button.is_active()
                && let Some(ui) = weak.upgrade()
            {
                ui.style.set(Style {
                    background: index,
                    ..ui.style.get()
                });
                ui.refresh_preview();
                ui.stack.set_visible_child_name("preview");
                ui.preview_label.set_label("Your display · snapshot");
            }
        });
    }
    let weak = Rc::downgrade(&ui);
    ui.follow.connect_active_notify(move |toggle| {
        if let Some(ui) = weak.upgrade() {
            ui.update_mouse_access(toggle.is_active());
            ui.style.set(Style {
                follow_cursor: toggle.is_active(),
                ..ui.style.get()
            });
            ui.play_preview();
            ui.stack.set_visible_child_name("preview");
            ui.preview_label.set_label("Your display · snapshot");
        }
    });
    let weak = Rc::downgrade(&ui);
    ui.preview.set_draw_func(move |_, cr, w, h| {
        if let Some(ui) = weak.upgrade() {
            ui.draw_preview(cr, w as f64, h as f64);
        }
    });
    let weak = Rc::downgrade(&ui);
    let preview_action = gio::SimpleAction::new("preview-camera", None);
    preview_action.connect_activate(move |_, _| {
        if let Some(ui) = weak.upgrade() {
            if ui.recording.borrow().is_some() || ui.rendering.get() || ui.countdown.get() > 0 {
                return;
            }
            ui.follow.set_active(true);
            ui.stack.set_visible_child_name("preview");
            ui.play_preview();
        }
    });
    app.add_action(&preview_action);
    replay.set_action_name(Some("app.preview-camera"));
    let weak = Rc::downgrade(&ui);
    refresh.connect_clicked(move |_| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.recording.borrow().is_some() || ui.rendering.get() || ui.countdown.get() > 0 {
            return;
        }
        ui.refreshing_preview.set(true);
        ui.window.set_visible(false);
        glib::timeout_add_local_once(Duration::from_millis(220), move || {
            match Preview::capture() {
                Ok(preview) => {
                    ui.preview_aspect
                        .set_ratio(preview.geometry.width as f32 / preview.geometry.height as f32);
                    *ui.preview_scene.borrow_mut() = Some(preview);
                    ui.stack.set_visible_child_name("preview");
                    ui.preview_label.set_label("Your display · snapshot");
                    ui.refresh_preview();
                }
                Err(error) => ui
                    .detail
                    .set_label(&format!("Could not refresh preview: {error}")),
            }
            ui.refreshing_preview.set(false);
            ui.window.present();
        });
    });
    let weak = Rc::downgrade(&ui);
    ui.primary.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.activate();
        }
    });
    let action = gio::SimpleAction::new("record", None);
    let weak = Rc::downgrade(&ui);
    action.connect_activate(move |_, _| {
        if let Some(ui) = weak.upgrade() {
            ui.activate();
        }
    });
    app.add_action(&action);
    app.set_accels_for_action("app.record", &["<Control><Shift>r"]);
    let weak = Rc::downgrade(&ui);
    ui.open.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade()
            && let Some(path) = ui.latest.borrow().as_ref()
        {
            ui.open_path(path);
        }
    });
    let weak = Rc::downgrade(&ui);
    folder.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            match fs::create_dir_all(video_directory()) {
                Ok(()) => ui.open_path(&video_directory()),
                Err(e) => ui
                    .detail
                    .set_label(&format!("Could not open recordings: {e}")),
            }
        }
    });
    let weak = Rc::downgrade(&ui);
    ui.window.connect_close_request(move |_| {
        if let Some(ui) = weak.upgrade()
            && (ui.recording.borrow().is_some() || ui.rendering.get())
        {
            ui.detail
                .set_label("Stop the recording and let the export finish before closing.");
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    ui.window.present();
    // The timer owns the workspace until its window closes; widget callbacks only hold weak references.
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if !ui.window.is_visible() && !ui.refreshing_preview.get() {
            return glib::ControlFlow::Break;
        }
        ui.tick();
        if ui.stack.visible_child_name().as_deref() == Some("video")
            && let Some(stream) = ui.video.media_stream()
            && let Some(error) = stream.error()
        {
            ui.stack.set_visible_child_name("preview");
            ui.preview_label.set_label("Your display · snapshot");
            ui.detail.set_label(&format!(
                "Video saved. Built-in playback unavailable ({error}). Use Open video."
            ));
        }
        while let Ok(event) = rx.try_recv() {
            match event {
                ExportEvent::MouseSession(session) => {
                    *ui.mouse_session.borrow_mut() = Some(session);
                    ui.mouse_note
                        .set_label("Mouse access ready for this app session.");
                }
                ExportEvent::Progress(value) => {
                    ui.progress.set_fraction(value);
                    ui.badge
                        .set_label(&format!("EXPORTING  {:02}%", (value * 100.0) as u32));
                    if value >= 0.98 {
                        ui.detail
                            .set_label("Finishing the video and preparing it for playback…");
                    }
                }
                ExportEvent::Finished(result) => {
                    ui.rendering.set(false);
                    ui.reset();
                    match result {
                        Ok((path, cleanup_warning)) => {
                            ui.status.set_label("That’s a wrap. Your video is ready.");
                            ui.detail.set_label(
                                &cleanup_warning
                                    .map(|warning| format!("Video saved. {warning}"))
                                    .unwrap_or_else(|| path.display().to_string()),
                            );
                            ui.video.set_file(Some(&gio::File::for_path(&path)));
                            ui.stack.set_visible_child_name("video");
                            ui.preview_label.set_label("Finished recording");
                            *ui.latest.borrow_mut() = Some(path);
                            ui.open.set_visible(true);
                            ui.badge.set_label("✓  SAVED");
                            ui.primary.set_label("●   Record another");
                            notify("Your screen just got its close-up. Video saved!");
                        }
                        Err(error) => {
                            ui.status.set_label("Export needs attention.");
                            ui.detail.set_label(&error);
                            ui.badge.set_label("EXPORT FAILED");
                            ui.open.set_visible(ui.latest.borrow().is_some());
                            notify("Export needs a hand. Recovery capture is in the cache.");
                        }
                    }
                }
            }
        }
        glib::ControlFlow::Continue
    });
}

impl Workspace {
    fn update_mouse_access(&self, enabled: bool) {
        if !enabled {
            if self.mouse_authorizing.replace(false) {
                self.mouse_session.borrow_mut().take();
                self.primary.set_sensitive(true);
            }
            self.mouse_note
                .set_label("Auto-camera is off. Clicks are not collected.");
            return;
        }
        if self.mouse_session.borrow().is_some() {
            self.mouse_note
                .set_label("Mouse access ready for this app session.");
            return;
        }
        match clicks::Session::authorize() {
            Ok(session) => {
                *self.mouse_session.borrow_mut() = Some(session);
                self.mouse_authorizing.set(true);
                self.primary.set_sensitive(false);
                self.mouse_note.set_label(
                    "Approve mouse access in the system prompt. Cancel to use pause-based zooms.",
                );
                notify("One click before the clicks: approve mouse access in the system prompt.");
            }
            Err(error) => self
                .mouse_note
                .set_label(&format!("{error} Using pause-based zooms.")),
        }
    }

    fn poll_mouse_access(&self) {
        let mut session = self.mouse_session.borrow_mut();
        if let Some(mouse) = session.as_mut() {
            mouse.poll();
            if let Some(error) = &mouse.error {
                self.mouse_note.set_label(error);
                session.take();
                if self.mouse_authorizing.replace(false) {
                    self.primary.set_sensitive(true);
                }
            } else if mouse.ready && self.mouse_authorizing.replace(false) {
                self.mouse_note.set_label(
                    "Click-triggered zooms ready. Access lasts until you close Screenema.",
                );
                self.primary.set_sensitive(true);
            }
        }
    }

    fn open_path(&self, path: &Path) {
        if let Err(error) = gio::AppInfo::launch_default_for_uri(
            &gio::File::for_path(path).uri(),
            None::<&gio::AppLaunchContext>,
        ) {
            self.detail
                .set_label(&format!("Could not open file: {error}"));
        }
    }

    fn reset(&self) {
        self.primary.set_sensitive(true);
        self.primary.set_label("●   Start recording");
        self.primary.remove_css_class("recording");
        self.settings.set_sensitive(true);
        self.preview_button.set_sensitive(true);
        self.refresh_button.set_sensitive(true);
        self.progress.set_visible(false);
        self.badge.set_label("●  READY");
    }

    fn activate(self: &Rc<Self>) {
        if self.rendering.get() || self.mouse_authorizing.get() {
            return;
        }
        if self.countdown.get() > 0 {
            self.countdown.set(0);
            self.reset();
            self.status.set_label("Take your time. Ready when you are.");
            self.detail
                .set_label("Countdown cancelled. Nothing was recorded.");
            return;
        }
        if self.recording.borrow().is_some() {
            self.stop();
            return;
        }
        let monitor = match focused_monitor() {
            Ok(monitor) => monitor,
            Err(error) => {
                self.status.set_label("Display unavailable");
                self.detail.set_label(&error);
                return;
            }
        };
        // Check required tools before the countdown, so a missing renderer cannot strand a take.
        for tool in ["gpu-screen-recorder", "ffmpeg", "ffprobe"] {
            if !std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .any(|dir| dir.join(tool).is_file())
            {
                self.status.set_label("One thing is missing.");
                self.detail
                    .set_label(&format!("Install {tool}, then try again."));
                return;
            }
        }
        if let Some(stream) = self.video.media_stream() {
            stream.pause();
        }
        self.stack.set_visible_child_name("preview");
        self.preview_label.set_label("Your display · snapshot");
        self.open.set_visible(false);
        self.settings.set_sensitive(false);
        self.preview_button.set_sensitive(false);
        self.refresh_button.set_sensitive(false);
        self.primary.set_label("Cancel countdown");
        self.countdown.set(3);
        let generation = self.countdown_generation.get() + 1;
        self.countdown_generation.set(generation);
        self.badge.set_label("STARTING IN  3");
        self.status.set_label("Set the scene.");
        self.detail.set_label(&format!(
            "Capturing {}. You can minimize this window after recording starts.",
            monitor.name
        ));
        let weak = Rc::downgrade(self);
        let mut monitor = Some(monitor);
        glib::timeout_add_local(Duration::from_secs(1), move || {
            let Some(ui) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let remaining = ui.countdown.get();
            if remaining == 0
                || !ui.window.is_visible()
                || ui.countdown_generation.get() != generation
            {
                return glib::ControlFlow::Break;
            }
            ui.countdown.set(remaining - 1);
            if remaining == 1 {
                ui.start(monitor.take().unwrap());
                return glib::ControlFlow::Break;
            }
            ui.badge
                .set_label(&format!("STARTING IN  {}", remaining - 1));
            glib::ControlFlow::Continue
        });
    }

    fn start(&self, monitor: FocusedMonitor) {
        let result = (|| -> Result<ActiveRecording, String> {
            let source = next_recording_path().map_err(|e| e.to_string())?;
            let log = fs::File::create(source.with_extension("log")).map_err(|e| e.to_string())?;
            let mut command = Command::new("gpu-screen-recorder");
            command.args([
                "-w",
                &monitor.name,
                "-cursor",
                "no",
                "-f",
                "60",
                "-fm",
                "cfr",
                "-c",
                "mp4",
                "-k",
                "h264",
                "-q",
                "ultra",
                "-tune",
                "quality",
                "-exclude-metadata",
                "yes",
                "-write-first-frame-ts",
                "yes",
            ]);
            if let Some(audio) =
                audio_source(self.system_audio.is_active(), self.microphone.is_active())
            {
                command.args(["-a", audio]);
            }
            let cursor_epoch = glib::monotonic_time();
            let child = command
                .arg("-o")
                .arg(&source)
                .stdout(Stdio::null())
                .stderr(log)
                .spawn()
                .map_err(|e| e.to_string())?;
            let started = Instant::now();
            let cursor_running = Arc::new(AtomicBool::new(true));
            let cursor_sampler = cursor_sampler(monitor.area, cursor_running.clone(), cursor_epoch);
            let mut clicks = if self.follow.is_active() {
                self.mouse_session.borrow_mut().take()
            } else {
                None
            };
            if let Some(session) = clicks.as_mut() {
                if let Err(error) = session.start() {
                    self.mouse_note
                        .set_label(&format!("{error} Using pause-based zooms."));
                    clicks = None;
                } else {
                    self.mouse_note
                        .set_label("Detecting mouse clicks while recording.");
                }
            }
            Ok(ActiveRecording {
                child,
                cursor_running,
                cursor_sampler,
                source,
                area: monitor.area,
                started,
                cursor_epoch,
                clicks,
            })
        })();
        match result {
            Ok(active) => {
                *self.latest.borrow_mut() = Some(active.source.clone());
                *self.recording.borrow_mut() = Some(active);
                self.primary.set_label("■   Stop & finish");
                self.primary.add_css_class("recording");
                self.status.set_label("You’re recording. Make it yours.");
                self.detail.set_label("Minimize this window for a clean take. Return here to stop with Ctrl + Shift + R.");
            }
            Err(error) => {
                self.reset();
                self.status.set_label("Could not start recording.");
                self.detail.set_label(&error);
            }
        }
    }

    fn tick(&self) {
        self.poll_mouse_access();
        let mut recording = self.recording.borrow_mut();
        if let Some(active) = recording.as_mut() {
            if let Some(session) = active.clicks.as_mut() {
                session.poll();
                if let Some(error) = &session.error {
                    self.mouse_note
                        .set_label(&format!("{error} This take will use pause-based zooms."));
                }
            }
            self.badge
                .set_label(&elapsed_label(active.started.elapsed()));
            if matches!(active.child.try_wait(), Ok(Some(_))) {
                let mut active = recording.take().unwrap();
                let end = glib::monotonic_time();
                if let Some(session) = active.clicks.as_mut() {
                    let _ = session.stop();
                }
                active.cursor_running.store(false, Ordering::Relaxed);
                self.reset();
                self.status.set_label("The recorder stopped unexpectedly.");
                self.detail.set_label(&format!("Check {} for the recorder’s error. Recovery data stays in the temporary cache.", active.source.with_extension("log").display()));
                // Avoid blocking GTK while a cursor query completes.
                let tx = self.tx.clone();
                thread::spawn(move || {
                    if let Some(mut session) = active.clicks.take()
                        && session.finish_recording(end).is_ok()
                    {
                        let _ = tx.send(ExportEvent::MouseSession(session));
                    }
                    let _ = active.cursor_sampler.join();
                });
                notify("Screenema stopped early. Check the recorder log.");
            }
        }
    }

    fn stop(&self) {
        let mut recording = self.recording.borrow_mut();
        let Some(active) = recording.as_mut() else {
            return;
        };
        if let Err(error) = stop_recorder(&mut active.child) {
            self.detail
                .set_label(&format!("Could not stop: {error}. Try again."));
            return;
        }
        let mut active = recording.take().unwrap();
        let end = glib::monotonic_time();
        if let Some(session) = active.clicks.as_mut()
            && let Err(error) = session.stop()
        {
            session.error = Some(error);
        }
        active.cursor_running.store(false, Ordering::Relaxed);
        self.rendering.set(true);
        self.primary.set_sensitive(false);
        self.primary.set_label("Finishing your video…");
        self.primary.remove_css_class("recording");
        self.status.set_label("Adding the finishing touches.");
        self.detail.set_label(
            "Rendering the camera move at full detail. Only the finished video will be kept.",
        );
        self.badge.set_label("EXPORTING");
        self.progress.set_fraction(0.0);
        self.progress.set_visible(true);
        let tx = self.tx.clone();
        let mut style = self.style.get();
        thread::spawn(move || {
            let mut active = active;
            let mut click_warning = None;
            let timestamps = if let Some(mut session) = active.clicks.take() {
                match session.finish_recording(end) {
                    Ok(timestamps) => {
                        style.click_camera = true;
                        let _ = tx.send(ExportEvent::MouseSession(session));
                        timestamps
                    }
                    Err(error) => {
                        click_warning = Some(format!(
                            "Click detection unavailable ({error}). Used pause-based zooms."
                        ));
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            let mut samples = active.cursor_sampler.join().unwrap_or_default();
            let result = (|| {
                let status = active.child.wait().map_err(|e| e.to_string())?;
                if !status.success() {
                    return Err(format!(
                        "Recorder could not finalize. See {}",
                        active.source.with_extension("log").display()
                    ));
                }
                let first_frame =
                    synchronize_cursor(&active.source, active.cursor_epoch, &mut samples)?;
                clicks::attach(&mut samples, &timestamps, first_frame);
                let output = cinematic_path(&active.source);
                render_cinematic(
                    &active.source,
                    &output,
                    active.area,
                    &samples,
                    style,
                    |value| {
                        let _ = tx.send(ExportEvent::Progress(value));
                    },
                )?;
                let cleanup_warning = cleanup_capture(&active.source)
                    .err()
                    .map(|e| format!("Temporary capture cleanup needs attention: {e}"));
                let warning = [click_warning, cleanup_warning]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" ");
                Ok((output, (!warning.is_empty()).then_some(warning)))
            })();
            let _ = tx.send(ExportEvent::Finished(result));
        });
    }
}

fn notify(message: &str) {
    let _ = Command::new("notify-send")
        .args(["Screenema", message])
        .spawn();
}

struct Preview {
    source: cairo::ImageSurface,
    background: cairo::ImageSurface,
    geometry: scene::Geometry,
    camera: scene::CameraPlan,
    samples: Vec<CursorSample>,
}

impl Preview {
    fn capture() -> Result<Self, String> {
        let monitor = focused_monitor()?;
        // Capture before presenting this window so the preview does not recursively contain itself.
        let shot = Command::new("grim")
            .args(["-o", &monitor.name, "-t", "png", "-"])
            .output()
            .map_err(|e| e.to_string())?;
        if !shot.status.success() {
            return Err("Display snapshot unavailable.".into());
        }
        let texture = gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(shot.stdout))
            .map_err(|e| e.to_string())?;
        let width = texture.width();
        let height = texture.height();
        let mut pixels = vec![0_u8; width as usize * height as usize * 4];
        texture.download(&mut pixels, width as usize * 4);
        let source = cairo::ImageSurface::create_for_data(
            pixels,
            cairo::Format::ARgb32,
            width,
            height,
            width * 4,
        )
        .map_err(|e| e.to_string())?;
        let geometry = scene::Geometry::new(width as u32, height as u32);
        let pointer = Command::new("hyprctl")
            .args(["cursorpos", "-j"])
            .output()
            .ok()
            .and_then(|o| serde_json::from_slice::<CursorPosition>(&o.stdout).ok())
            .map(|p| {
                (
                    ((p.x - monitor.area.x) / monitor.area.width * width as f64)
                        .clamp(0.0, width as f64),
                    ((p.y - monitor.area.y) / monitor.area.height * height as f64)
                        .clamp(0.0, height as f64),
                )
            })
            .unwrap_or((width as f64 * 0.6, height as f64 * 0.5));
        let area = CaptureArea {
            x: 0.0,
            y: 0.0,
            width: width as f64,
            height: height as f64,
        };
        let samples: Vec<_> = (0..240)
            .map(|i| CursorSample {
                click: false,
                elapsed: Duration::from_millis(i * 25),
                x: pointer.0
                    + ((i as f64 * 0.025 - 2.2) / 0.4).clamp(0.0, 1.0)
                        * (width as f64
                            * if pointer.0 < width as f64 * 0.5 {
                                0.8
                            } else {
                                0.2
                            }
                            - pointer.0),
                y: pointer.1,
            })
            .collect();
        Ok(Self {
            source,
            background: scene::backdrop(geometry, Style::default())?,
            geometry,
            camera: scene::CameraPlan::new(geometry, area, &samples, 6.0, true, false),
            samples,
        })
    }
}

impl Workspace {
    fn refresh_preview(self: &Rc<Self>) {
        if let Some(preview) = self.preview_scene.borrow_mut().as_mut()
            && let Ok(background) = scene::backdrop(preview.geometry, self.style.get())
        {
            preview.background = background;
        }
        self.play_preview();
    }

    fn play_preview(self: &Rc<Self>) {
        self.preview_started.set(Instant::now());
        self.preview.queue_draw();
        if !self.style.get().follow_cursor {
            return;
        }
        let started = self.preview_started.get();
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(16), move || {
            let Some(ui) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if ui.preview_started.get() != started
                || !ui.window.is_visible()
                || !ui.style.get().follow_cursor
            {
                return glib::ControlFlow::Break;
            }
            ui.preview.queue_draw();
            if started.elapsed().as_secs_f64() > 6.0 {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    fn draw_preview(&self, cr: &cairo::Context, width: f64, height: f64) {
        let preview = self.preview_scene.borrow();
        let Some(preview) = preview.as_ref() else {
            cr.set_source_rgb(0.65, 0.68, 0.65);
            cr.set_font_size(14.0);
            cr.move_to(24.0, 48.0);
            let _ = cr.show_text("Display preview unavailable. Check that grim is installed.");
            return;
        };
        let scale =
            (width / preview.geometry.width as f64).min(height / preview.geometry.height as f64);
        let _ = cr.save();
        cr.translate(
            (width - preview.geometry.width as f64 * scale) / 2.0,
            (height - preview.geometry.height as f64 * scale) / 2.0,
        );
        cr.scale(scale, scale);
        let time = if self.style.get().follow_cursor {
            self.preview_started.get().elapsed().as_secs_f64().min(6.0)
        } else {
            0.0
        };
        let _ = scene::draw_scene(
            cr,
            &preview.source,
            &preview.background,
            preview.geometry,
            preview.camera.at(time),
            interpolated_cursor(&preview.samples, Duration::from_secs_f64(time), &mut 0)
                .map(|sample| (sample.x, sample.y)),
        );
        let _ = cr.restore();
    }
}
