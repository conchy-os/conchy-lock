//! Wayland side: ext-session-lock surfaces (or one xdg window in `--demo`),
//! keyboard input, SHM buffers and the frame loop.

use std::{rc::Rc, time::{Duration, Instant}};

use anyhow::{anyhow, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_output, delegate_registry, delegate_seat,
    delegate_session_lock, delegate_shm, delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            channel::{self, Event as ChannelEvent},
            timer::{TimeoutAction, Timer},
            EventLoop, LoopHandle,
        },
        calloop_wayland_source::WaylandSource,
        client::{
            globals::registry_queue_init,
            protocol::{wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface},
            Connection, Proxy, QueueHandle,
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        Capability, SeatHandler, SeatState,
    },
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
        SessionLockSurfaceConfigure,
    },
    shell::{
        xdg::{
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
            XdgShell,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use tiny_skia::{Pixmap, PixmapPaint, Transform};
use zeroize::Zeroizing;

use crate::{
    auth::{self, AuthResult, Backend},
    background::{self, Background},
    scene::{Phase, Scene, UiState, REGION_H, REGION_W},
    theme::{LockConfig, Palette},
};

pub struct Options {
    pub demo: bool,
    pub wallpaper: Option<std::path::PathBuf>,
}

enum Kind {
    Lock(#[allow(dead_code)] SessionLockSurface),
    Window(#[allow(dead_code)] Window),
}

struct Surf {
    wl: wl_surface::WlSurface,
    _kind: Kind,
    output: Option<wl_output::WlOutput>,
    /// logical size, 0 until the first configure
    size: (u32, u32),
    scale: i32,
    pool: Option<SlotPool>,
    bg: Option<Rc<Background>>,
    frame_pending: bool,
    dirty: bool,
}

pub struct App {
    conn: Connection,
    qh: QueueHandle<App>,
    loop_handle: LoopHandle<'static, App>,

    registry: RegistryState,
    outputs: OutputState,
    seats: SeatState,
    shm: Shm,
    compositor: CompositorState,
    lock_state: Option<SessionLockState>,
    xdg: Option<XdgShell>,
    session_lock: Option<SessionLock>,
    keyboard: Option<wl_keyboard::WlKeyboard>,

    surfs: Vec<Surf>,

    cfg: LockConfig,
    pal: Palette,
    wallpaper: Option<image::RgbaImage>,
    scene: Scene,

    ui: UiState,
    password: Zeroizing<String>,
    user: String,
    backend: Backend,
    auth_tx: channel::Sender<AuthResult>,

    locked: bool,
    exit: bool,
    exit_error: Option<String>,
}

pub fn run(opts: Options) -> Result<()> {
    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();
    let event_loop: EventLoop<App> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle.clone())
        .map_err(|e| anyhow!("wayland source: {e}"))?;

    let mut cfg = LockConfig::load();
    if let Some(w) = opts.wallpaper {
        cfg.wallpaper = Some(w);
    }
    let pal = Palette::load();
    let user = std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_else(|_| "user".into());
    let wallpaper = background::load_wallpaper(&cfg);

    let (auth_tx, auth_rx) = channel::channel::<AuthResult>();
    loop_handle
        .insert_source(auth_rx, |ev, _, app| {
            if let ChannelEvent::Msg(r) = ev {
                app.on_auth(r);
            }
        })
        .map_err(|e| anyhow!("auth channel: {e}"))?;

    // Clock: wake every second, redraw only when the minute text changed.
    loop_handle
        .insert_source(Timer::from_duration(Duration::from_secs(1)), |_, _, app| {
            app.tick();
            TimeoutAction::ToDuration(Duration::from_secs(1))
        })
        .map_err(|e| anyhow!("timer: {e}"))?;

    let now = Instant::now();
    let (time, date) = clock_strings();
    let mut app = App {
        conn: conn.clone(),
        qh: qh.clone(),
        loop_handle: loop_handle.clone(),
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        seats: SeatState::new(&globals, &qh),
        shm: Shm::bind(&globals, &qh)?,
        compositor: CompositorState::bind(&globals, &qh)?,
        lock_state: None,
        xdg: None,
        session_lock: None,
        keyboard: None,
        surfs: Vec::new(),
        scene: Scene::new(pal.clone(), user.clone()),
        cfg,
        pal,
        wallpaper,
        ui: UiState {
            time,
            date,
            len: 0,
            phase: Phase::Idle,
            caps: false,
            attempts: 0,
            message: None,
            started: now,
            phase_since: now,
            last_key: now - Duration::from_secs(10),
        },
        password: Zeroizing::new(String::new()),
        user,
        backend: if opts.demo { Backend::Demo } else { Backend::pam() },
        auth_tx,
        locked: false,
        exit: false,
        exit_error: None,
    };

    // Let outputs/seats announce themselves before we lock.
    let mut eq_dummy = event_loop; // keep ownership explicit
    eq_dummy.dispatch(Duration::from_millis(50), &mut app)?;
    eq_dummy.dispatch(Duration::from_millis(50), &mut app)?;

    if opts.demo {
        app.xdg = Some(XdgShell::bind(&globals, &qh).map_err(|e| anyhow!("xdg_shell: {e}"))?);
        app.create_demo_window();
    } else {
        let state = SessionLockState::new(&globals, &qh);
        let lock = state.lock(&qh).map_err(|e| anyhow!("compositor has no ext-session-lock: {e}"))?;
        app.lock_state = Some(state);
        app.session_lock = Some(lock);
        // Create surfaces right away: the compositor only confirms `locked`
        // once every output shows one of ours.
        let outs: Vec<_> = app.outputs.outputs().collect();
        for o in outs {
            app.create_lock_surface(o);
        }
    }

    while !app.exit {
        eq_dummy.dispatch(None, &mut app)?;
    }

    match app.exit_error {
        Some(e) => Err(anyhow!(e)),
        None => Ok(()),
    }
}


/// Composes the repainted centre region: background crop + scene layer with
/// the intro fade. Returns (region pixmap, x, y, still_animating).
pub fn compose(bg: &Background, scene: &mut Scene, ui: &UiState, scale: u32, now: Instant) -> (Pixmap, u32, u32, bool) {
    let (pw, ph) = (bg.width, bg.height);
    let rw = ((REGION_W * scale as f32) as u32).min(pw);
    let rh = ((REGION_H * scale as f32) as u32).min(ph);
    let (rx, ry) = ((pw - rw) / 2, (ph - rh) / 2);

    let mut layer = Pixmap::new(rw, rh).unwrap();
    let animating = scene.draw(&mut layer, ui, scale as f32, now);

    let mut base = Pixmap::new(rw, rh).unwrap();
    for row in 0..rh as usize {
        let src = ((ry as usize + row) * pw as usize + rx as usize) * 4;
        let dst = row * rw as usize * 4;
        base.data_mut()[dst..dst + rw as usize * 4].copy_from_slice(&bg.rgba.data()[src..src + rw as usize * 4]);
    }
    let fade = (now.duration_since(ui.started).as_secs_f32() / 0.35).clamp(0.0, 1.0);
    let opacity = 1.0 - (1.0 - fade).powi(3);
    base.draw_pixmap(0, 0, layer.as_ref(), &PixmapPaint { opacity, ..Default::default() }, Transform::identity(), None);
    (base, rx, ry, animating)
}

/// Offline preview (`--png`): same pipeline, no Wayland.
pub fn render_png(path: &std::path::Path, w: u32, h: u32, scale: u32, wallpaper: Option<std::path::PathBuf>, variant: &str) -> Result<()> {
    let mut cfg = LockConfig::load();
    if let Some(w) = wallpaper {
        cfg.wallpaper = Some(w);
    }
    let pal = Palette::load();
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    let wp = background::load_wallpaper(&cfg);
    let bg = background::build(wp.as_ref(), w * scale, h * scale, scale as f32, &cfg, &pal);
    let mut scene = Scene::new(pal, user);

    let now = Instant::now();
    let long_ago = now - Duration::from_secs(60);
    let (time, date) = clock_strings();
    let mut ui = UiState {
        time, date, len: 0, phase: Phase::Idle, caps: false, attempts: 0, message: None,
        started: long_ago, phase_since: now, last_key: long_ago,
    };
    match variant {
        "typing" => ui.len = 9,
        "checking" => { ui.len = 9; ui.phase = Phase::Checking; ui.phase_since = now - Duration::from_millis(300); }
        "failed" => { ui.phase = Phase::Failed; ui.attempts = 2; ui.message = Some("Wrong password · 2 attempts".into()); ui.phase_since = now - Duration::from_millis(700); }
        "caps" => { ui.len = 3; ui.caps = true; }
        _ => {}
    }
    let (region, rx, ry, _) = compose(&bg, &mut scene, &ui, scale, now);
    let mut full = bg.rgba.clone();
    full.draw_pixmap(rx as i32, ry as i32, region.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
    full.save_png(path).map_err(|e| anyhow!("png: {e}"))?;
    Ok(())
}

fn clock_strings() -> (String, String) {
    let n = chrono::Local::now();
    (n.format("%H:%M").to_string(), n.format("%A, %-d %B").to_string())
}

impl App {
    fn create_lock_surface(&mut self, output: wl_output::WlOutput) {
        let Some(lock) = &self.session_lock else { return };
        if self.surfs.iter().any(|s| s.output.as_ref() == Some(&output)) {
            return;
        }
        let wl = self.compositor.create_surface(&self.qh);
        let scale = self.outputs.info(&output).map(|i| i.scale_factor).unwrap_or(1).max(1);
        let ls = lock.create_lock_surface(wl.clone(), &output, &self.qh);
        wl.set_buffer_scale(scale);
        self.surfs.push(Surf {
            wl,
            _kind: Kind::Lock(ls),
            output: Some(output),
            size: (0, 0),
            scale,
            pool: None,
            bg: None,
            frame_pending: false,
            dirty: true,
        });
    }

    fn create_demo_window(&mut self) {
        let Some(xdg) = &self.xdg else { return };
        let wl = self.compositor.create_surface(&self.qh);
        let window = xdg.create_window(wl.clone(), WindowDecorations::ServerDefault, &self.qh);
        window.set_title("conchy-lock (demo — password: demo)");
        window.set_app_id("org.conchy.lock.demo");
        window.set_min_size(Some((800, 600)));
        window.commit();
        self.surfs.push(Surf {
            wl,
            _kind: Kind::Window(window),
            output: None,
            size: (0, 0),
            scale: 1,
            pool: None,
            bg: None,
            frame_pending: false,
            dirty: true,
        });
    }

    fn idx(&self, wl: &wl_surface::WlSurface) -> Option<usize> {
        self.surfs.iter().position(|s| &s.wl == wl)
    }

    fn tick(&mut self) {
        let (time, date) = clock_strings();
        if time != self.ui.time || date != self.ui.date {
            self.ui.time = time;
            self.ui.date = date;
            self.redraw_all();
        }
    }

    fn redraw_all(&mut self) {
        for i in 0..self.surfs.len() {
            self.request_draw(i);
        }
    }

    /// Draw now unless a frame callback is outstanding (then it will draw).
    fn request_draw(&mut self, i: usize) {
        let s = &mut self.surfs[i];
        if s.size.0 == 0 {
            return;
        }
        if s.frame_pending {
            s.dirty = true;
        } else {
            self.draw(i);
        }
    }

    fn draw(&mut self, i: usize) {
        let now = Instant::now();
        let (lw, lh, scale) = {
            let s = &self.surfs[i];
            (s.size.0, s.size.1, s.scale.max(1) as u32)
        };
        let (pw, ph) = (lw * scale, lh * scale);
        if pw == 0 || ph == 0 {
            return;
        }

        // Background: built lazily, cached per (physical) size.
        let stale = self.surfs[i].bg.as_ref().map_or(true, |b| b.width != pw || b.height != ph);
        if stale {
            let bg = background::build(self.wallpaper.as_ref(), pw, ph, scale as f32, &self.cfg, &self.pal);
            self.surfs[i].bg = Some(Rc::new(bg));
        }
        let bg = self.surfs[i].bg.clone().unwrap();

        let (base, rx, ry, animating) = compose(&bg, &mut self.scene, &self.ui, scale, now);
        let (rw, rh) = (base.width(), base.height());
        let fade_anim = now.duration_since(self.ui.started).as_secs_f32() < 0.35;

        let s = &mut self.surfs[i];
        let pool = s.pool.get_or_insert_with(|| {
            SlotPool::new((pw * ph * 4) as usize * 2, &self.shm).expect("shm pool")
        });
        let stride = pw as i32 * 4;
        let Ok((buffer, canvas)) = pool.create_buffer(pw as i32, ph as i32, stride, wl_shm::Format::Xrgb8888) else {
            tracing::warn!("shm buffer allocation failed");
            return;
        };
        canvas[..bg.bgra.len()].copy_from_slice(&bg.bgra);
        for row in 0..rh as usize {
            let dst = ((ry as usize + row) * pw as usize + rx as usize) * 4;
            let src = row * rw as usize * 4;
            let out = &mut canvas[dst..dst + rw as usize * 4];
            out.copy_from_slice(&base.data()[src..src + rw as usize * 4]);
            for px in out.chunks_exact_mut(4) {
                px.swap(0, 2);
                px[3] = 255;
            }
        }

        s.wl.set_buffer_scale(s.scale);
        s.wl.damage_buffer(0, 0, pw as i32, ph as i32);
        if animating || fade_anim {
            s.wl.frame(&self.qh, s.wl.clone());
            s.frame_pending = true;
        }
        s.dirty = false;
        if buffer.attach_to(&s.wl).is_err() {
            tracing::warn!("attach failed");
        }
        s.wl.commit();
    }

    // ---------------------------------------------------------------- input

    fn handle_key(&mut self, ev: KeyEvent) {
        if self.ui.phase == Phase::Checking || self.ui.phase == Phase::Success {
            return;
        }
        let ctrl = false;
        let _ = ctrl;
        match ev.keysym {
            Keysym::Return | Keysym::KP_Enter => self.submit(),
            Keysym::BackSpace => {
                if self.password.pop().is_some() {
                    self.after_edit();
                }
            }
            Keysym::Escape => {
                if !self.password.is_empty() {
                    self.password.clear();
                    self.after_edit();
                }
            }
            _ => {
                if let Some(text) = ev.utf8 {
                    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
                    if !clean.is_empty() {
                        self.password.push_str(&clean);
                        self.ui.last_key = Instant::now();
                        self.after_edit();
                    }
                }
            }
        }
    }

    fn after_edit(&mut self) {
        self.ui.len = self.password.chars().count();
        if self.ui.phase == Phase::Failed {
            self.ui.phase = Phase::Idle;
        }
        self.ui.message = None;
        self.redraw_all();
    }

    fn submit(&mut self) {
        if self.password.is_empty() {
            return;
        }
        self.ui.phase = Phase::Checking;
        self.ui.phase_since = Instant::now();
        self.ui.message = None;
        let pw = std::mem::take(&mut self.password);
        auth::spawn(self.backend.clone(), self.user.clone(), pw, self.auth_tx.clone());
        self.redraw_all();
    }

    fn on_auth(&mut self, r: AuthResult) {
        let now = Instant::now();
        match r {
            AuthResult::Ok => {
                self.ui.phase = Phase::Success;
                self.ui.phase_since = now;
                self.redraw_all();
                self.unlock_and_exit();
            }
            AuthResult::Denied | AuthResult::Error(_) => {
                if let AuthResult::Error(e) = &r {
                    tracing::error!("PAM error: {e}");
                }
                self.ui.attempts += 1;
                self.ui.phase = Phase::Failed;
                self.ui.phase_since = now;
                self.ui.len = 0;
                self.ui.message = Some(if self.ui.attempts > 1 {
                    format!("Wrong password · {} attempts", self.ui.attempts)
                } else {
                    "Wrong password".to_owned()
                });
                self.redraw_all();
            }
        }
    }

    fn unlock_and_exit(&mut self) {
        if let Some(lock) = self.session_lock.take() {
            lock.unlock();
            // Make sure the unlock request actually reaches the compositor
            // before we exit, otherwise the session stays locked.
            let _ = self.conn.roundtrip();
        }
        self.exit = true;
    }
}

// ------------------------------------------------------------------ traits --

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &wl_surface::WlSurface, new_factor: i32) {
        if let Some(i) = self.idx(surface) {
            self.surfs[i].scale = new_factor.max(1);
            self.surfs[i].bg = None;
            self.request_draw(i);
        }
    }
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &wl_surface::WlSurface, _: u32) {
        if let Some(i) = self.idx(surface) {
            self.surfs[i].frame_pending = false;
            // Either something changed while waiting, or the scene is animating
            // (draw() re-arms the callback while it still is).
            self.draw(i);
        }
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        // Hot-plugged monitor while locked: cover it immediately.
        self.create_lock_surface(output);
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.surfs.retain(|s| s.output.as_ref() != Some(&output));
    }
}

impl SessionLockHandler for App {
    fn locked(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        tracing::info!("session locked");
        self.locked = true;
    }
    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        self.exit_error = Some(if self.locked {
            "session lock was terminated by the compositor".into()
        } else {
            "compositor refused the session lock (is another locker running?)".into()
        });
        self.exit = true;
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: SessionLockSurface, configure: SessionLockSurfaceConfigure, _serial: u32) {
        let wl = surface.wl_surface().clone();
        if let Some(i) = self.idx(&wl) {
            self.surfs[i].size = configure.new_size;
            self.surfs[i].bg = None;
            self.request_draw(i);
        }
    }
}

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, window: &Window, configure: WindowConfigure, _: u32) {
        let w = configure.new_size.0.map_or(1280, |v| v.get());
        let h = configure.new_size.1.map_or(800, |v| v.get());
        if let Some(i) = self.idx(window.wl_surface()) {
            self.surfs[i].size = (w, h);
            self.surfs[i].bg = None;
            self.request_draw(i);
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            match self.seats.get_keyboard_with_repeat(
                qh,
                &seat,
                None,
                self.loop_handle.clone(),
                Box::new(|app: &mut App, _kbd, ev| app.handle_key(ev)),
            ) {
                Ok(kb) => self.keyboard = Some(kb),
                Err(e) => tracing::error!("keyboard: {e}"),
            }
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Keyboard {
            if let Some(kb) = self.keyboard.take() {
                kb.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {}
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, ev: KeyEvent) {
        self.handle_key(ev);
    }
    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, ev: KeyEvent) {
        self.handle_key(ev);
    }
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, m: Modifiers, _: RawModifiers, _: u32) {
        if self.ui.caps != m.caps_lock {
            self.ui.caps = m.caps_lock;
            self.redraw_all();
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_seat!(App);
delegate_keyboard!(App);
delegate_session_lock!(App);
delegate_xdg_shell!(App);
delegate_xdg_window!(App);
delegate_registry!(App);

// `Proxy` is used for `.id()`-style comparisons in some sctk versions.
#[allow(unused)]
fn _assert_proxy<T: Proxy>() {}
