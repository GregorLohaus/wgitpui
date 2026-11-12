//! wgitpui — a GPU-rendered git client.
//!
//! Usage:
//!   wgitpui [REPO_PATH...]   from a terminal: opens in the running app (or starts it in the
//!                            background); each path becomes a tab; previous tabs are restored
//!   wgitpui --foreground ...  stay attached to the terminal
//!   wgitpui --screenshot out.png [--size 1440x900] [--scale 2] [--commit N] [--palette QUERY] [REPO_PATH]

mod app;
mod attr;
mod chrome;
mod cli;
mod diffview;
mod fileview;
mod palette;
mod clipboard;
mod font;
mod git;
mod gpu;
mod graph;
mod png;
mod rebase;
mod resolve;
mod syntax;
mod workspace;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key as WKey, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

use ui::{Cursor, Key, KeyPress, Ui};

struct Running {
    window: Arc<Window>,
    gpu: gpu::Gpu,
    cursor: Cursor,
}

struct Shell {
    ui: Ui,
    ws: workspace::Workspace,
    run: Option<Running>,
    last_click: Option<(Instant, (f32, f32))>,
    wake_at: Option<Instant>,
}

impl Shell {
    fn scale(&self) -> f32 {
        self.run.as_ref().map_or(1.0, |r| r.window.scale_factor() as f32)
    }

    fn redraw(&mut self) {
        let Some(run) = &mut self.run else { return };
        let size = run.window.inner_size();
        let scale = run.window.scale_factor() as f32;
        self.ui.begin(size.width as f32 / scale, size.height as f32 / scale, scale);
        self.ws.frame(&mut self.ui);
        run.gpu.frame(&mut self.ui.p, ui::theme::BG);
        self.ui.input.end_frame();

        if self.ui.cursor != run.cursor {
            run.cursor = self.ui.cursor;
            run.window.set_cursor(match run.cursor {
                Cursor::Default => CursorIcon::Default,
                Cursor::Pointer => CursorIcon::Pointer,
                Cursor::Text => CursorIcon::Text,
                Cursor::ResizeH => CursorIcon::ColResize,
                Cursor::Grab => CursorIcon::Grab,
                Cursor::Grabbing => CursorIcon::Grabbing,
            });
        }
        run.window.set_title(&self.ws.title());
        chrome::place_traffic_lights(&run.window, workspace::TAB_H);
        if self.ui.drag_window {
            self.ui.input.down = false; // the OS swallows the mouse-up during a window drag
            let _ = run.window.drag_window();
        }
        if self.ui.toggle_maximize {
            run.window.set_maximized(!run.window.is_maximized());
        }
        if self.ui.animating {
            run.window.request_redraw();
        }
        self.wake_at = self.ui.wake_at;
    }
}

fn map_key(k: &WKey) -> Option<Key> {
    Some(match k {
        WKey::Named(n) => match n {
            NamedKey::Enter => Key::Enter,
            NamedKey::Escape => Key::Escape,
            NamedKey::Backspace => Key::Backspace,
            NamedKey::Delete => Key::Delete,
            NamedKey::ArrowLeft => Key::Left,
            NamedKey::ArrowRight => Key::Right,
            NamedKey::ArrowUp => Key::Up,
            NamedKey::ArrowDown => Key::Down,
            NamedKey::Home => Key::Home,
            NamedKey::End => Key::End,
            NamedKey::PageUp => Key::PageUp,
            NamedKey::PageDown => Key::PageDown,
            NamedKey::Tab => Key::Tab,
            _ => return None,
        },
        WKey::Character(s) => Key::Char(s.chars().next()?.to_ascii_lowercase()),
        _ => return None,
    })
}

/// Events from outside the event loop.
enum UserEvent {
    /// "Open this repository" from the command line (single-instance socket).
    Open(PathBuf),
}

impl ApplicationHandler<UserEvent> for Shell {
    fn user_event(&mut self, _el: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Open(path) => {
                self.ws.open_tab(&path);
                if let Some(r) = &self.run {
                    r.window.set_minimized(false);
                    r.window.focus_window();
                    r.window.request_redraw();
                }
            }
        }
    }

    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.run.is_some() {
            return;
        }
        let attrs = chrome::window_attributes(Window::default_attributes())
            .with_title(self.ws.title())
            .with_inner_size(LogicalSize::new(1440.0, 900.0))
            .with_min_inner_size(LogicalSize::new(900.0, 560.0))
            ;
        let window = Arc::new(el.create_window(attrs).expect("failed to create window"));
        let gpu = match gpu::Gpu::new(window.clone()) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("wgitpui: {e}");
                el.exit();
                return;
            }
        };
        window.request_redraw();
        self.run = Some(Running { window, gpu, cursor: Cursor::Default });
    }

    fn new_events(&mut self, _el: &ActiveEventLoop, cause: StartCause) {
        if let StartCause::ResumeTimeReached { .. } = cause {
            self.wake_at = None;
            if let Some(r) = &self.run {
                r.window.request_redraw();
            }
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        el.set_control_flow(match self.wake_at {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        });
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let scale = self.scale();
        let input = &mut self.ui.input;
        let mut redraw = true;
        match event {
            WindowEvent::CloseRequested => {
                el.exit();
                return;
            }
            WindowEvent::Resized(size) => {
                if let Some(r) = &mut self.run {
                    r.gpu.resize(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => {
                self.redraw();
                return;
            }
            WindowEvent::CursorMoved { position, .. } => {
                input.mouse = (position.x as f32 / scale, position.y as f32 / scale);
            }
            WindowEvent::CursorLeft { .. } => input.mouse = (-1000.0, -1000.0),
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Right, .. } => {
                input.right_pressed = true;
            }
            // Ctrl-click is a secondary click on macOS.
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. }
                if cfg!(target_os = "macos") && input.mods.ctrl =>
            {
                input.right_pressed = true;
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => match state {
                ElementState::Pressed => {
                    input.down = true;
                    input.pressed = true;
                    let now = Instant::now();
                    let m = input.mouse;
                    if let Some((t, p)) = self.last_click {
                        if now - t < Duration::from_millis(400) && (p.0 - m.0).abs() < 4.0 && (p.1 - m.1).abs() < 4.0 {
                            input.double = true;
                        }
                    }
                    self.last_click = if input.double { None } else { Some((now, m)) };
                }
                ElementState::Released => {
                    input.down = false;
                    input.released = true;
                }
            },
            WindowEvent::MouseWheel { delta, .. } => match delta {
                MouseScrollDelta::LineDelta(x, y) => {
                    input.scroll.0 += x * 48.0;
                    input.scroll.1 += y * 48.0;
                    input.scroll_precise = false;
                }
                MouseScrollDelta::PixelDelta(p) => {
                    input.scroll.0 += p.x as f32 / scale;
                    input.scroll.1 += p.y as f32 / scale;
                    input.scroll_precise = true;
                }
            },
            WindowEvent::ModifiersChanged(m) => {
                let s = m.state();
                input.mods = ui::Mods {
                    shift: s.shift_key(),
                    ctrl: s.control_key(),
                    alt: s.alt_key(),
                    logo: s.super_key(),
                };
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if let Some(key) = map_key(&event.logical_key) {
                    input.keys.push(KeyPress { key, mods: input.mods });
                }
                if !input.mods.primary() && !input.mods.ctrl {
                    if let Some(t) = &event.text {
                        input.text.push_str(t.as_str());
                    }
                }
            }
            WindowEvent::Focused(true) => self.ws.on_focus(),
            // Dropping a folder (or any file inside a repository) opens it in a tab.
            WindowEvent::DroppedFile(path) => self.ws.open_tab(&path),
            WindowEvent::ScaleFactorChanged { .. } => {}
            _ => redraw = false,
        }
        if redraw {
            if let Some(r) = &self.run {
                r.window.request_redraw();
            }
        }
    }
}

struct Args {
    repos: Vec<PathBuf>,
    screenshot: Option<PathBuf>,
    size: (u32, u32),
    scale: f32,
    commit: Option<usize>,
    palette: Option<String>,
    files: bool,
    open: Option<String>,
    compare: Option<(usize, usize)>,
    split: Option<bool>,
    wide: bool,
    menu: Option<String>,
    resolve: Option<(String, String)>,
    push_menu: bool,
    /// Stay attached to the terminal instead of moving to the background.
    foreground: bool,
}

fn parse_args() -> Args {
    let mut a = Args { repos: vec![], screenshot: None, size: (1440, 900), scale: 2.0, commit: None, palette: None, files: false, open: None, compare: None, split: None, wide: false, menu: None, resolve: None, push_menu: false, foreground: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--screenshot" => a.screenshot = it.next().map(PathBuf::from),
            "--size" => {
                if let Some((w, h)) = it.next().as_deref().and_then(|s| s.split_once('x')) {
                    a.size = (w.parse().unwrap_or(1440), h.parse().unwrap_or(900));
                }
            }
            "--scale" => a.scale = it.next().and_then(|s| s.parse().ok()).unwrap_or(2.0),
            "--palette" => a.palette = it.next(),
            "--files" => a.files = true,
            "-f" | "--foreground" => a.foreground = true,
            // Older macOS passes a process serial number when launched from Finder.
            s if s.starts_with("-psn_") => {}
            "--compare" => {
                a.compare = it.next().and_then(|s| {
                    let (x, y) = s.split_once(',')?;
                    Some((x.parse().ok()?, y.parse().ok()?))
                })
            }
            "--split" => a.split = Some(true),
            "--unified" => a.split = Some(false),
            "--wide" => a.wide = true,
            "--menu" => a.menu = it.next(),
            "--push-menu" => a.push_menu = true,
            "--resolve" => {
                a.resolve = it.next().map(|s| match s.split_once(':') {
                    Some((p, c)) => (p.to_string(), c.to_string()),
                    None => (s, String::new()),
                })
            }
            "--open" => a.open = it.next(),
            "--commit" => a.commit = it.next().and_then(|s| s.parse().ok()),
            "-h" | "--help" => {
                println!("usage: wgitpui [-f|--foreground] [REPO ...]");
                println!();
                println!("From a terminal, opens REPO (default: the current directory) in the running app,");
                println!("or starts the app in the background so the shell stays usable. -f keeps it attached.");
                println!();
                println!("debug: wgitpui [--screenshot out.png [--size WxH] [--scale S] [--commit N] [--palette QUERY] [--files] [--open PATH] [--compare A,B] [--split|--unified] [--wide] [--menu BRANCH] [--resolve PATH[:o,t,b,-]] [--push-menu]] [REPO]");
                std::process::exit(0);
            }
            _ => a.repos.push(PathBuf::from(arg)),
        }
    }
    a
}

fn main() {
    let mut args = parse_args();
    // Make repository paths absolute: they may be handed to another process.
    let cwd = std::env::current_dir().unwrap_or_default();
    args.repos = args.repos.iter().map(|p| if p.is_absolute() { p.clone() } else { cwd.join(p) }).collect();

    // From a terminal: open the repository and give the shell back.
    use std::io::IsTerminal;
    if args.screenshot.is_none() && !args.foreground && std::io::stdin().is_terminal() {
        let targets = if args.repos.is_empty() { vec![cwd.clone()] } else { args.repos.clone() };
        for t in &targets {
            if let Err(e) = git::Repo::open(t) {
                eprintln!("wgitpui: {}: not a git repository ({})", t.display(), e.lines().next().unwrap_or(""));
                std::process::exit(1);
            }
        }
        if cli::send_to_running(&targets) {
            return;
        }
        if let Err(e) = cli::launch_detached(&targets) {
            eprintln!("wgitpui: couldn't start in the background ({e}); run with --foreground");
            std::process::exit(1);
        }
        return;
    }
    let fonts = match font::Fonts::load() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("wgitpui: {e}");
            std::process::exit(1);
        }
    };
    let mut ui = Ui::new(fonts);
    if let Some(path) = args.screenshot.clone() {
        // Screenshots never touch the saved session: open exactly the given repositories.
        let repos = if args.repos.is_empty() { vec![std::env::current_dir().unwrap_or_default()] } else { args.repos.clone() };
        let mut ws = workspace::Workspace::for_paths(&repos);
        let (w, h) = args.size;
        let s = args.scale;
        if let Some(app) = ws.active_mut() {
            app.finish_loading();
        }
        // Lay out once so the graph knows its viewport before selection scrolls it.
        ui.begin(w as f32, h as f32, s);
        ws.frame(&mut ui);
        ui.input.end_frame();
        if let Some(app) = ws.active_mut() {
            if let Some(i) = args.commit {
                app.select_row(i);
            }
            if let Some((a, b)) = args.compare {
                app.debug_compare(a, b, args.split, args.wide);
            } else if args.split.is_some() || args.wide {
                app.debug_compare(usize::MAX, usize::MAX, args.split, args.wide);
            }
            if let Some((p, c)) = &args.resolve {
                app.debug_resolve(p, c);
            }
            if args.push_menu {
                app.debug_push_menu((f32::MAX, workspace::TAB_H + app::TOP_H + 6.0));
            }
            if let Some(m) = &args.menu {
                app.debug_branch_menu(m, (520.0, 180.0));
            }
            if args.files || args.open.is_some() {
                app.show_files(args.open.clone());
            }
            if let Some(q) = &args.palette {
                app.open_palette(q);
            }
        }
        // A few frames let smooth-scroll and layout settle exactly as they would on screen.
        // Background work (e.g. line attribution) is waited for, up to a few seconds.
        for i in 0..120 {
            ui.input.mouse = (-1000.0, -1000.0);
            ui.begin(w as f32, h as f32, s);
            ws.frame(&mut ui);
            ui.input.end_frame();
            if i >= 3 && ui.wake_at.is_none_or(|t| t > std::time::Instant::now() + std::time::Duration::from_secs(1)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        if std::env::var_os("WGITPUI_FAKE_LIGHTS").is_some() {
            // Stand-ins for the macOS traffic lights at their measured positions (layout checks).
            for (x, c) in [(27.0, 0xff5f57), (50.0, 0xfebc2e), (73.0, 0x28c840)] {
                ui.p.circle((x, workspace::TAB_H * 0.5), 6.0, ui::Color::hex(c));
            }
        }
        let phys = ((w as f32 * s) as u32, (h as f32 * s) as u32);
        if let Err(e) = gpu::render_png(&mut ui.p, phys, ui::theme::BG, &path) {
            eprintln!("wgitpui: screenshot failed: {e}");
            std::process::exit(1);
        }
        println!("wrote {}", path.display());
        return;
    }

    // Each repository given on the command line opens in a tab (on top of the restored session).
    let mut repos = args.repos.into_iter();
    let mut ws = workspace::Workspace::new(repos.next());
    for r in repos {
        ws.open_tab(&r);
    }

    let event_loop = EventLoop::<UserEvent>::with_user_event().build().expect("failed to create event loop");
    // Single instance: later `wgitpui` invocations hand their repository to this window.
    let proxy = event_loop.create_proxy();
    cli::listen(move |p| {
        let _ = proxy.send_event(UserEvent::Open(p));
    });
    let mut shell = Shell { ui, ws, run: None, last_click: None, wake_at: None };
    event_loop.run_app(&mut shell).expect("event loop error");
}
