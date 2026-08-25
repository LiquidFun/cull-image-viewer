//! cull -- a fast GPU image culling tool.
//!
//! Window, GPU surface and input plumbing. All behaviour lives in [`cull::app`],
//! [`cull::view`] and friends, which are unit-tested without a display; this file is the
//! shell that drives them.

use std::sync::Arc;
use std::time::Duration;

use cull::app::{Action, App, DEFAULT_RADIUS};
use cull::gpu::Renderer;
use cull::prefetch::FileLoader;
use cull::trash::SystemBin;
use cull::ui;
use cull::view::Viewport;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

/// Sent by the prefetch ring when a decode finishes, so the event loop can collect it.
#[derive(Debug, Clone, Copy)]
struct Wake;

/// Textures held on the GPU. One more than the prefetch window so the image being
/// displayed is never the one evicted.
const TEXTURE_CACHE: usize = DEFAULT_RADIUS * 2 + 2;

/// Uploads accepted before the rest are left in the ring for a later frame. Each holds
/// its decoded pixels (~102 MB) until the transfer finishes, so this bounds that too.
const MAX_IN_FLIGHT: usize = 4;

/// Process start, for the startup breakdown logged at `RUST_LOG=info`. The number that
/// matters is not any single phase but when a photo is actually visible.
static LAUNCH: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

fn main() {
    std::sync::LazyLock::force(&LAUNCH);
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let root = match std::env::args().nth(1) {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            eprintln!("usage: cull <directory>");
            std::process::exit(2);
        }
    };
    if !root.is_dir() {
        eprintln!("not a directory: {}", root.display());
        std::process::exit(2);
    }

    // Shared between the decode workers, which draw pixel buffers from it, and the
    // renderer, which returns each one after uploading it. Sized to the uploads that can
    // be in flight at once, so it never holds more than the pipeline already does.
    let pixels = Arc::new(cull::decode::BufferPool::new(MAX_IN_FLIGHT));

    // Startup is phase-timed because the expensive part is machine-dependent -- the
    // library scan is I/O bound and the GPU init is driver bound, and which dominates
    // cannot be guessed. `RUST_LOG=info cull <dir>` prints the breakdown.
    let t0 = std::time::Instant::now();
    let app = App::new(
        &root,
        FileLoader::new(Arc::clone(&pixels)),
        Arc::new(SystemBin),
        DEFAULT_RADIUS,
        0,
    );
    log::info!("startup: scanned {} images in {:?}", app.len(), t0.elapsed());
    println!("{} images under {}", app.len(), root.display());
    if app.is_empty() {
        eprintln!("nothing to show");
        std::process::exit(1);
    }

    let event_loop: EventLoop<Wake> = match EventLoop::with_user_event().build() {
        Ok(el) => el,
        Err(e) => {
            // Most often there is simply no display, e.g. a plain SSH session. A clear
            // message beats a panic and a backtrace.
            eprintln!("cannot open a window: {e}");
            eprintln!("cull needs a Wayland or X11 display (WAYLAND_DISPLAY or DISPLAY).");
            std::process::exit(1);
        }
    };
    // Wait, not Poll: the ring wakes us through an EventLoopProxy the moment a decode
    // finishes, so there is nothing to poll for. Polling would spin a core and, worse,
    // hammer the ring's mutex and starve the very worker threads we are waiting on.
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    app.prefetch().set_waker(Arc::new(move || {
        // Failure just means the loop is shutting down.
        let _ = proxy.send_event(Wake);
    }));
    let mut shell = Shell::new(app, pixels);
    if let Err(e) = event_loop.run_app(&mut shell) {
        eprintln!("event loop error: {e}");
        std::process::exit(1);
    }
}

/// GPU objects that only exist once a window does.
struct Graphics {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
}

struct Shell {
    app: App,
    gfx: Option<Graphics>,
    /// Cursor position in physical pixels, for cursor-anchored zoom.
    cursor: (f64, f64),
    dragging: bool,
    last_drag: Option<(f64, f64)>,
    /// Physical-pixel rect of the image area (x, y, w, h), i.e. the window minus egui's
    /// panels. Reported by egui each frame, because the sidebar is resizable and its
    /// width cannot be assumed.
    image_rect: (f32, f32, f32, f32),
    /// Row under the pointer and when it got there, so a hover only triggers a preload
    /// once it has settled. Sweeping the pointer down a list would otherwise thrash the
    /// decoders with work the user never asked for.
    hover: Option<(usize, std::time::Instant)>,
    /// Index most recently handed to the prefetcher as a hint.
    hinted: Option<usize>,
    /// Directories the user has collapsed. Owned here because the virtualised list
    /// cannot rely on egui's own collapsing state.
    collapsed: std::collections::HashSet<std::path::PathBuf>,
    /// Tree scroll offset carried between frames, so the selection can be kept in view
    /// with a minimal correction rather than being re-centred every time.
    tree_offset: f32,
    /// Index the tree has already scrolled to. The list only follows the selection when
    /// this disagrees with it, so scrolling is corrected on a *move* and not on every
    /// frame -- otherwise the offset is re-asserted continuously and the user cannot
    /// scroll away from whatever is selected.
    followed: Option<usize>,
    /// Recycled pixel buffers, shared with the decode workers. Held here because the
    /// renderer that returns them does not exist until a window does.
    pixels: Arc<cull::decode::BufferPool>,
    /// Whether a frame containing an actual photo has been presented yet, so the
    /// startup timing is logged once.
    first_image_shown: bool,
}

/// How long the pointer must rest on a row before it is preloaded.
const HOVER_DELAY: std::time::Duration = std::time::Duration::from_millis(60);

impl Shell {
    fn new(app: App, pixels: Arc<cull::decode::BufferPool>) -> Self {
        Self {
            app,
            pixels,
            gfx: None,
            cursor: (0.0, 0.0),
            dragging: false,
            last_drag: None,
            image_rect: (0.0, 0.0, 1.0, 1.0),
            hover: None,
            hinted: None,
            collapsed: std::collections::HashSet::new(),
            tree_offset: 0.0,
            followed: None,
            first_image_shown: false,
        }
    }

    /// Apply the debounced hover hint. Returns true if a later frame is needed to
    /// re-check a hover that has not yet settled.
    fn update_hover(&mut self, hovered: Option<usize>) -> bool {
        let now = std::time::Instant::now();
        match hovered {
            None => {
                self.hover = None;
                if self.hinted.take().is_some() {
                    self.app.prefetch().clear_hint();
                }
                false
            }
            Some(index) => {
                // Restart the clock whenever the row under the pointer changes.
                let since = match self.hover {
                    Some((i, at)) if i == index => at,
                    _ => {
                        self.hover = Some((index, now));
                        now
                    }
                };
                if now.duration_since(since) < HOVER_DELAY {
                    // Not settled yet; ask for another frame to re-check.
                    return true;
                }
                if self.hinted != Some(index) {
                    self.app.prefetch().hint(index);
                    self.hinted = Some(index);
                }
                false
            }
        }
    }

    /// Cursor position relative to the centre of the image area, which is what the view
    /// transform expects for cursor-anchored zoom.
    fn cursor_relative(&self) -> (f64, f64) {
        let (x, y, w, h) = self.image_rect;
        (
            self.cursor.0 - f64::from(x) - f64::from(w) / 2.0,
            self.cursor.1 - f64::from(y) - f64::from(h) / 2.0,
        )
    }

    /// Viewport available to the image, from the rect egui reported.
    fn image_viewport(&self) -> Viewport {
        Viewport::new(f64::from(self.image_rect.2), f64::from(self.image_rect.3))
    }

    /// True when the pointer is over the image rather than a panel.
    fn cursor_over_image(&self) -> bool {
        let (x, y, w, h) = self.image_rect;
        self.cursor.0 >= f64::from(x)
            && self.cursor.0 < f64::from(x + w)
            && self.cursor.1 >= f64::from(y)
            && self.cursor.1 < f64::from(y + h)
    }

    /// Move newly decoded images onto the GPU and bind the current one.
    ///
    /// Returns true when a redraw is warranted: either something was uploaded, or work
    /// was deferred and needs another frame. Deferred work matters because the ring's
    /// wake event has already fired, so without asking for a frame the leftovers would
    /// sit uncollected until the next user input.
    fn pump_uploads(&mut self) -> bool {
        let Some(gfx) = self.gfx.as_mut() else {
            return false;
        };
        let current = self.app.index();
        let mut ready = self.app.prefetch().take_ready();
        if ready.is_empty() {
            return false;
        }

        // Nearest-first, so the image being looked at is uploaded before its neighbours.
        ready.sort_by_key(|(i, _)| i.abs_diff(current));

        // A 25.6 MP texture is ~102 MB, and `write_texture` costs a CPU memcpy into a
        // staging buffer plus the transfer -- around 14 ms, most of a 60 Hz frame. A
        // fixed count per frame therefore stalls on slow hardware and wastes time on
        // fast hardware, so spend a wall-clock budget instead and let the rest wait.
        // Deferred pixels stay in the ring, so nothing is lost.
        // Accepting pixels is cheap now -- it only creates a texture and records the
        // rows. The transfer cost is paid by `pump` under a frame budget. Still cap how
        // many are taken at once so the pending list, and the CPU pixels it holds,
        // stay bounded.
        let mut uploaded = 0;
        let mut deferred = 0;
        for (index, image) in ready {
            let in_flight = gfx.renderer.cache.pending_len() + uploaded;
            if in_flight >= MAX_IN_FLIGHT && index != current {
                // Hand it back so a later frame picks it up.
                self.app.prefetch().put_back(index, image);
                deferred += 1;
                continue;
            }
            // Creates the texture and queues the rows; the transfer itself happens in
            // bands under a frame budget, so no single frame pays for a whole 102 MB copy.
            gfx.renderer
                .cache
                .queue_upload(&gfx.device, index, image);
            uploaded += 1;
        }
        // Keep the window's textures plus the hovered one. Without the hint exemption a
        // hovered row far from the cursor had its texture evicted the instant after it
        // was uploaded, and because the ring had already handed over the pixels it would
        // never redo the work -- so clicking that row showed nothing, for ever.
        gfx.renderer
            .cache
            .retain_window_with(self.app.index(), DEFAULT_RADIUS + 1, self.hinted);

        // Repair: the renderer owns the only copy of the pixels once uploaded, so if the
        // texture for the current image is gone the ring has to decode it again.
        let current = self.app.index();
        if !gfx.renderer.cache.contains(current)
            && matches!(
                self.app.prefetch().state(current),
                cull::prefetch::State::Collected | cull::prefetch::State::Absent
            )
        {
            self.app.prefetch().require(current);
        }

        // Once the current texture is resident, tell the app its real dimensions so the
        // view can be fitted. Only do it when the bound image actually changed.
        let index = self.app.index();
        let needs_note = self
            .app
            .shown()
            .is_none_or(|s| s.index != index);
        if let Some(entry) = gfx.renderer.cache.get(index) {
            let (stored, orientation, colour) =
                (entry.stored, entry.orientation.value(), entry.colour);
            if needs_note {
                let vp = self.image_viewport();
                self.app.viewport = vp;
                self.app.note_shown(index, stored, orientation, None, None);
            }
            // Always, even when the view was already laid out from the header: the
            // profile only exists once the file has actually been decoded, and without
            // this R10's non-sRGB warning could never fire.
            self.app.note_colour(index, colour);
        }
        uploaded > 0 || deferred > 0
    }

    fn apply(&mut self, effects: cull::app::Effects) {
        if effects.tree_changed {
            // Indices shifted, so every cached texture is now suspect.
            if let Some(gfx) = self.gfx.as_mut() {
                gfx.renderer.cache.clear();
            }
        }
        if effects.redraw {
            if let Some(gfx) = self.gfx.as_ref() {
                gfx.window.request_redraw();
            }
        }
    }

    fn key_action(key: &Key) -> Option<Action> {
        Some(match key {
            Key::Named(NamedKey::ArrowRight | NamedKey::ArrowDown | NamedKey::Space) => {
                Action::Next
            }
            Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowUp) => Action::Prev,
            Key::Named(NamedKey::PageDown) => Action::Skip(10),
            Key::Named(NamedKey::PageUp) => Action::Skip(-10),
            Key::Named(NamedKey::Home) => Action::First,
            Key::Named(NamedKey::End) => Action::Last,
            Key::Named(NamedKey::Enter) => Action::ConfirmDelete,
            // Plain Delete. It only *arms* the deletion; Enter is what carries it out,
            // so the deliberate second keystroke is still there without needing a
            // modifier on the first.
            Key::Named(NamedKey::Delete) => Action::Delete,
            Key::Character(c) => match c.as_str() {
                // X toggles the fit mode, as in geeqie (R6).
                "x" | "X" => Action::ToggleFitMode,
                "f" | "F" => Action::Fit,
                "z" | "Z" => Action::ActualSize,
                "u" | "U" => Action::Undo,
                _ => return None,
            },
            _ => return None,
        })
    }
}

impl ApplicationHandler<Wake> for Shell {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("cull")
            .with_inner_size(winit::dpi::LogicalSize::new(1600.0, 1000.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("failed to create window: {e}");
                std::process::exit(1);
            }
        };

        let t = std::time::Instant::now();
        // Vulkan only by default, rather than `Instance::default()`'s "probe everything".
        // Enumerating the GL backend spins up an EGL/GLX context purely to be discarded,
        // and this is Linux-with-Vulkan by requirement (R2). `WGPU_BACKEND=gl` still
        // overrides, so the fallback is a flag away rather than gone.
        let instance = wgpu::Instance::new(
            wgpu::InstanceDescriptor {
                backends: wgpu::Backends::VULKAN,
                ..wgpu::InstanceDescriptor::new_without_display_handle()
            }
            // `with_env` still honours WGPU_BACKEND, so the GL fallback is a flag away.
            .with_env(),
        );
        let surface = match instance.create_surface(window.clone()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("failed to create a GPU surface: {e}");
                std::process::exit(1);
            }
        };
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .unwrap_or_else(|e| {
            eprintln!("no suitable GPU adapter: {e}");
            eprintln!("cull needs a Vulkan-capable GPU driver.");
            std::process::exit(1);
        });
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("cull device"),
            required_features: wgpu::Features::empty(),
            // A 25.6 MP image is 6192 px wide, so the default 8192 limit suffices.
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        }))
        .unwrap_or_else(|e| {
            eprintln!("failed to create a GPU device: {e}");
            std::process::exit(1);
        });
        log::info!(
            "startup: GPU adapter + device in {:?} ({})",
            t.elapsed(),
            adapter.get_info().name
        );

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        // Prefer an sRGB surface so the hardware does the linear<->sRGB conversion.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        if !format.is_srgb() {
            log::warn!("no sRGB surface format available; colours may be slightly off");
        }
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            // One, not the default two. Each queued frame is another vsync interval
            // between a keystroke and the pixels appearing, and this program draws a
            // single quad -- there is no GPU work worth pipelining a frame ahead for.
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&device, &config);

        let t = std::time::Instant::now();
        let renderer = Renderer::new(&device, format, TEXTURE_CACHE, Arc::clone(&self.pixels));
        log::info!("startup: render pipeline in {:?}", t.elapsed());
        let egui_ctx = egui::Context::default();
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui_ctx.viewport_id(),
            &window,
            None,
            None,
            None,
        );
        let egui_renderer = egui_wgpu::Renderer::new(
            &device,
            format,
            egui_wgpu::RendererOptions::default(),
        );

        self.app
            .resize(f64::from(config.width), f64::from(config.height));

        window.request_redraw();

        self.gfx = Some(Graphics {
            window,
            surface,
            device,
            queue,
            config,
            renderer,
            egui_ctx,
            egui_state,
            egui_renderer,
        });
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(gfx) = self.gfx.as_mut() else {
            return;
        };

        // Let egui consume input aimed at the sidebar first.
        let response = gfx.egui_state.on_window_event(&gfx.window, &event);
        let egui_wants = response.consumed;
        if response.repaint {
            gfx.window.request_redraw();
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                let gfx = self.gfx.as_mut().expect("graphics exists");
                gfx.config.width = size.width.max(1);
                gfx.config.height = size.height.max(1);
                gfx.surface.configure(&gfx.device, &gfx.config);
                // The image area is recomputed during redraw, once egui has laid out its
                // panels against the new size, so just ask for one.
                gfx.window.request_redraw();
            }

            WindowEvent::CursorMoved { position, .. } => {
                let prev = self.cursor;
                self.cursor = (position.x, position.y);
                if self.dragging && !egui_wants && self.cursor_over_image() {
                    let delta = (self.cursor.0 - prev.0, self.cursor.1 - prev.1);
                    let e = self.app.pan(delta);
                    self.apply(e);
                }
                self.last_drag = Some(self.cursor);
            }

            WindowEvent::MouseInput { state, button, .. } => {
                if button == MouseButton::Left && !egui_wants && self.cursor_over_image() {
                    self.dragging = state == ElementState::Pressed;
                } else if state == ElementState::Released {
                    // Always end a drag, even if it finished over a panel.
                    self.dragging = false;
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                // Scrolling the sidebar must scroll the list, not zoom the image.
                if egui_wants || !self.cursor_over_image() {
                    return;
                }
                // Accumulate both wheel and trackpad deltas into detents. Line deltas
                // are already in detents; pixel deltas are scaled to a sensible rate.
                let detents = match delta {
                    MouseScrollDelta::LineDelta(_, y) => f64::from(y),
                    MouseScrollDelta::PixelDelta(p) => p.y / 120.0,
                };
                if detents != 0.0 {
                    let cursor = self.cursor_relative();
                    let e = self.app.zoom(detents, cursor);
                    self.apply(e);
                }
            }

            WindowEvent::KeyboardInput { event, .. } => {
                if egui_wants || event.state != ElementState::Pressed {
                    return;
                }
                if event.logical_key == Key::Named(NamedKey::Escape) {
                    // Escape backs out of an armed delete first; only quit if there is
                    // nothing pending, so it cannot both cancel and exit.
                    if self.app.delete_pending() {
                        let e = self.app.act(Action::Cancel);
                        self.apply(e);
                    } else {
                        event_loop.exit();
                    }
                    return;
                }
                if let Some(action) = Self::key_action(&event.logical_key) {
                    let e = self.app.act(action);
                    self.apply(e);
                }
            }

            WindowEvent::RedrawRequested => self.redraw(),

            _ => {}
        }
    }

    /// A decode finished. Upload it and redraw; no polling involved.
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: Wake) {
        if self.pump_uploads() {
            if let Some(gfx) = self.gfx.as_ref() {
                gfx.window.request_redraw();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // While a navigation key is held the loop must wake once navigation goes quiet,
        // so the full prefetch window can be restored. Otherwise sleep until something
        // happens: an input, or the ring signalling a finished decode.
        match self.app.settle_deadline() {
            Some(deadline) => event_loop.set_control_flow(ControlFlow::WaitUntil(deadline)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
        let e = self.app.tick();
        if e.redraw {
            if let Some(gfx) = self.gfx.as_ref() {
                gfx.window.request_redraw();
            }
        }
    }
}

impl Shell {
    fn redraw(&mut self) {
        let more_work = self.pump_uploads();
        let Some(gfx) = self.gfx.as_mut() else {
            return;
        };

        use wgpu::CurrentSurfaceTexture as Cst;
        let frame = match gfx.surface.get_current_texture() {
            // Suboptimal still presents correctly; reconfiguring every frame would be
            // worse than a slightly stale swapchain.
            Cst::Success(f) | Cst::Suboptimal(f) => f,
            // Recoverable: reconfigure and let the next frame try again.
            Cst::Lost | Cst::Outdated => {
                gfx.surface.configure(&gfx.device, &gfx.config);
                return;
            }
            // Transient; skip this frame.
            Cst::Timeout | Cst::Occluded => return,
            Cst::Validation => {
                log::error!("surface acquisition failed validation");
                return;
            }
        };
        let target = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = gfx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });

        // --- UI logic first: egui owns the layout, so only it can say how much room is
        // left for the image. Its meshes are drawn later, on top.
        let raw_input = gfx.egui_state.take_egui_input(&gfx.window);
        let ctx = gfx.egui_ctx.clone();
        let mut avail = egui::Rect::NOTHING;
        let mut hovered = None;
        let mut next_offset = self.tree_offset;
        // Only chase the selection when it has actually moved since the tree last
        // followed it. Otherwise the correction is re-applied on every frame and snaps
        // the list back as soon as the user scrolls anywhere else.
        let follow = self.followed != Some(self.app.index());
        let full_output = ctx.run_ui(raw_input, |root| {
            let outcome = ui::draw(root, &self.app, &self.collapsed, self.tree_offset, follow);
            avail = outcome.image_rect;
            next_offset = outcome.scroll_offset;
            if let Some(dir) = outcome.toggled_dir {
                if !self.collapsed.remove(&dir) {
                    self.collapsed.insert(dir);
                }
            }
            if let Some(index) = outcome.selected {
                self.app.select(index);
                // Clicking a row must not then scroll the list under the pointer: the
                // user picked something they could already see.
                self.followed = Some(self.app.index());
            }
            hovered = outcome.hovered;
            if let Some(action) = outcome.action {
                self.app.act(action);
            }
        });
        if follow {
            self.followed = Some(self.app.index());
        }
        self.tree_offset = next_offset;
        // Hovering a row is a strong signal the user is about to click it, so preload it
        // once the pointer settles (R12).
        let hover_pending = self.update_hover(hovered);

        // The rect egui did not claim is the image area. Converted to physical pixels,
        // this drives both the view transform and the render-pass viewport, so the aspect
        // ratio is correct regardless of how wide the user drags the sidebar.
        let ppp = full_output.pixels_per_point;
        let gfx = self.gfx.as_mut().expect("graphics exists");
        let max_w = gfx.config.width as f32;
        let max_h = gfx.config.height as f32;
        self.image_rect = (
            (avail.min.x * ppp).clamp(0.0, max_w),
            (avail.min.y * ppp).clamp(0.0, max_h),
            (avail.width() * ppp).clamp(1.0, max_w),
            (avail.height() * ppp).clamp(1.0, max_h),
        );

        // Keep the app's viewport in step, so fit and clamping use the real area.
        let viewport = self.image_viewport();
        if (viewport.width - self.app.viewport.width).abs() > 0.5
            || (viewport.height - self.app.viewport.height).abs() > 0.5
        {
            self.app.resize(viewport.width, viewport.height);
        }

        // --- Move a bounded slice of pending pixel data to the GPU. This is the only
        // place a frame spends time on transfers, and it is capped.
        const BAND_BUDGET: Duration = Duration::from_millis(4);
        let gfx = self.gfx.as_mut().expect("graphics exists");
        let bands_left =
            gfx.renderer
                .cache
                .pump(&gfx.queue, self.app.index(), BAND_BUDGET);

        // --- Image layer.
        let drew = gfx.renderer.draw(
            &gfx.queue,
            &mut encoder,
            &target,
            self.app.index(),
            &self.app.view,
            viewport,
            self.image_rect,
        );
        if !drew {
            // Nothing resident yet: clear so the previous frame is not left stale.
            encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.05,
                            g: 0.05,
                            b: 0.05,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }

        // --- UI layer: draw the meshes produced above.
        let gfx = self.gfx.as_mut().expect("graphics exists");
        gfx.egui_state
            .handle_platform_output(&gfx.window, full_output.platform_output);
        let tris = gfx
            .egui_ctx
            .tessellate(full_output.shapes, full_output.pixels_per_point);
        for (id, delta) in &full_output.textures_delta.set {
            gfx.egui_renderer
                .update_texture(&gfx.device, &gfx.queue, *id, delta);
        }
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [gfx.config.width, gfx.config.height],
            pixels_per_point: full_output.pixels_per_point,
        };
        gfx.egui_renderer.update_buffers(
            &gfx.device,
            &gfx.queue,
            &mut encoder,
            &tris,
            &screen,
        );
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("egui pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &target,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            gfx.egui_renderer.render(&mut pass, &tris, &screen);
        }
        for id in &full_output.textures_delta.free {
            gfx.egui_renderer.free_texture(id);
        }

        gfx.queue.submit(Some(encoder.finish()));
        frame.present();

        // Something is on screen, so the neighbours can be decoded now. Doing this in
        // `App::new` instead put 21 full-resolution decodes on every core while the
        // driver was initialising and the first frame was trying to appear.
        self.app.warm_full_window();
        if drew && !self.first_image_shown {
            self.first_image_shown = true;
            log::info!("startup: first image on screen {:?} after launch", LAUNCH.elapsed());
        }

        // Another frame is needed to drain deferred uploads, or to re-check a hover that
        // has not yet settled.
        if more_work || hover_pending || bands_left {
            gfx.window.request_redraw();
        }
    }
}
