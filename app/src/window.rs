//! The window: a tao event loop, one window with the system webview (wry), and on macOS
//! the menu bar (muda). The webview loads only the `oli` custom protocol, may not
//! navigate anywhere else or open windows, keeps nothing on disk, and reaches the app
//! core only through the IPC handler, whose messages the event loop hands to
//! [`ipc::Core::dispatch`] on the main thread.

use open_live_ingest_app::{ipc, navigation, protocol};
use std::borrow::Cow;
use std::process::ExitCode;
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use wry::http::{Request, Response};
use wry::{NewWindowResponse, WebContext, WebViewBuilder};

const TITLE: &str = "Open Live Ingest";

/// What other threads and callbacks hand to the event loop.
enum UserEvent {
    /// A message the page posted with `window.ipc.postMessage`.
    Ipc(String),
    /// A script for the page from the core: a reply that came later, or an event.
    Deliver(String),
    /// A menu item was chosen.
    #[cfg(target_os = "macos")]
    Menu(muda::MenuEvent),
}

pub fn run() -> ExitCode {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();

    let window = match WindowBuilder::new()
        .with_title(TITLE)
        .with_inner_size(LogicalSize::new(720.0, 640.0))
        .with_min_inner_size(LogicalSize::new(480.0, 400.0))
        .build(&event_loop)
    {
        Ok(window) => window,
        Err(e) => {
            tracing::error!("could not open the window: {e}");
            return ExitCode::FAILURE;
        }
    };

    #[cfg(target_os = "macos")]
    let menu_bar = menu::MenuBar::new();
    #[cfg(target_os = "macos")]
    {
        let proxy = event_loop.create_proxy();
        muda::MenuEvent::set_event_handler(Some(move |event| {
            let _ = proxy.send_event(UserEvent::Menu(event));
        }));
    }

    let mut web_context = WebContext::new(webview_data_dir());
    let ipc_proxy = event_loop.create_proxy();
    let webview = WebViewBuilder::new_with_web_context(&mut web_context)
        .with_custom_protocol(protocol::SCHEME.to_string(), |_id, request| {
            asset_response(&request)
        })
        .with_navigation_handler(|url| {
            let allowed = navigation::is_allowed(&url);
            if !allowed {
                // The URL is not logged: the page has no business going anywhere, and
                // whatever it tried may carry something it should not.
                tracing::warn!("navigation away from the app refused");
            }
            allowed
        })
        .with_new_window_req_handler(|_url, _features| {
            tracing::warn!("new window refused");
            NewWindowResponse::Deny
        })
        .with_download_started_handler(|_url, _path| false)
        .with_ipc_handler(move |request: Request<String>| {
            let _ = ipc_proxy.send_event(UserEvent::Ipc(request.into_body()));
        })
        // Nothing is kept: no cookies, storage or cache between launches (spec, section 6).
        .with_incognito(true)
        .with_url(navigation::start_url())
        .build(&window);
    let webview = match webview {
        Ok(webview) => webview,
        Err(e) => {
            tracing::error!("could not create the webview: {e}");
            return ExitCode::FAILURE;
        }
    };

    // The core answers some commands later, from its own threads; the proxy brings
    // what it delivers back to this loop, where the webview lives.
    let deliver_proxy = std::sync::Mutex::new(event_loop.create_proxy());
    let deliver: ipc::Deliver = std::sync::Arc::new(move |script| {
        let proxy = deliver_proxy.lock().unwrap_or_else(|e| e.into_inner());
        let _ = proxy.send_event(UserEvent::Deliver(script));
    });
    let mut core = ipc::Core::new(open_live_ingest::config::default_path(), deliver);
    tracing::info!(version = open_live_ingest_app::VERSION, "app started");

    event_loop.run(move |event, _target, control_flow| {
        // Kept alive for as long as the loop runs.
        let _ = (&window, &web_context);
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {
                // After tao has finished launching, or its default menu replaces this one.
                #[cfg(target_os = "macos")]
                menu_bar.install();
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => request_quit(control_flow),
            Event::UserEvent(UserEvent::Ipc(raw)) => {
                if let Some((reply, effect)) = core.dispatch(&raw) {
                    if let Err(e) = webview.evaluate_script(&reply.script()) {
                        tracing::warn!("could not deliver a reply to the page: {e}");
                    }
                    if effect == ipc::Effect::Quit {
                        request_quit(control_flow);
                    }
                }
            }
            Event::UserEvent(UserEvent::Deliver(script)) => {
                if let Err(e) = webview.evaluate_script(&script) {
                    tracing::warn!("could not deliver a message to the page: {e}");
                }
            }
            #[cfg(target_os = "macos")]
            Event::UserEvent(UserEvent::Menu(event)) if menu_bar.is_quit(&event) => {
                request_quit(control_flow)
            }
            #[cfg(target_os = "macos")]
            Event::UserEvent(UserEvent::Menu(event)) if menu_bar.is_show_logs(&event) => {
                tracing::info!("menu: show logs");
                if let Err(message) = core.show_log(ipc::LogFile::App) {
                    tracing::warn!("show logs failed: {message}");
                }
            }
            _ => {}
        }
    })
}

/// Every way of quitting comes here: the close button, Cmd-W, Cmd-Q and the app menu's
/// Quit, and the page's `quit` command. Nothing can be live yet, so this quits; the
/// question asked while live (spec, section 8) goes here.
fn request_quit(control_flow: &mut ControlFlow) {
    tracing::info!("quitting");
    *control_flow = ControlFlow::Exit;
}

/// The protocol handler's answer as an HTTP response for wry.
fn asset_response(request: &Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let asset = protocol::serve(request.uri().path());
    let mut response = Response::builder().status(asset.status);
    for (name, value) in asset.headers() {
        response = response.header(name, value);
    }
    response
        .body(Cow::Borrowed(asset.body))
        .expect("static status and headers are valid")
}

/// WebView2 keeps a user-data folder even in private mode, and by default puts it next
/// to the executable, which is not writable under `Program Files`. So on Windows it is
/// `%LOCALAPPDATA%\open-live-ingest\webview`. macOS needs none.
fn webview_data_dir() -> Option<std::path::PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(|base| {
            std::path::PathBuf::from(base)
                .join("open-live-ingest")
                .join("webview")
        })
}

/// The macOS menu bar. Windows has none (spec, section 5).
#[cfg(target_os = "macos")]
mod menu {
    use muda::accelerator::{Accelerator, Code, Modifiers};
    use muda::{AboutMetadata, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

    pub struct MenuBar {
        menu: Menu,
        window_menu: Submenu,
        help_menu: Submenu,
        quit: MenuItem,
        show_logs: MenuItem,
    }

    impl MenuBar {
        pub fn new() -> Self {
            // Quit is the app's own item rather than the predefined one, which would end
            // the process without asking; it goes through the quit guard instead.
            let quit = MenuItem::new(
                "Quit Open Live Ingest",
                true,
                Some(Accelerator::new(Modifiers::META, Code::KeyQ)),
            );
            let about = AboutMetadata {
                name: Some(super::TITLE.to_string()),
                version: Some(open_live_ingest_app::VERSION.to_string()),
                ..Default::default()
            };
            let separator = PredefinedMenuItem::separator;
            let app_menu = Submenu::with_items(
                super::TITLE,
                true,
                &[
                    &PredefinedMenuItem::about(None, Some(about)),
                    &separator(),
                    &PredefinedMenuItem::services(None),
                    &separator(),
                    &PredefinedMenuItem::hide(None),
                    &PredefinedMenuItem::hide_others(None),
                    &PredefinedMenuItem::show_all(None),
                    &separator(),
                    &quit,
                ],
            )
            .expect("app menu");
            // Cmd-C and Cmd-V reach a text field in the webview only through these.
            let edit_menu = Submenu::with_items(
                "Edit",
                true,
                &[
                    &PredefinedMenuItem::undo(None),
                    &PredefinedMenuItem::redo(None),
                    &separator(),
                    &PredefinedMenuItem::cut(None),
                    &PredefinedMenuItem::copy(None),
                    &PredefinedMenuItem::paste(None),
                    &PredefinedMenuItem::select_all(None),
                ],
            )
            .expect("edit menu");
            let window_menu = Submenu::with_items(
                "Window",
                true,
                &[
                    &PredefinedMenuItem::minimize(None),
                    &PredefinedMenuItem::close_window(None),
                ],
            )
            .expect("window menu");
            // Reveals app.log in Finder (spec, section 7).
            let show_logs = MenuItem::new("Show logs", true, None);
            let help_menu = Submenu::with_items("Help", true, &[&show_logs]).expect("help menu");
            let menu = Menu::with_items(&[&app_menu, &edit_menu, &window_menu, &help_menu])
                .expect("menu bar");
            MenuBar {
                menu,
                window_menu,
                help_menu,
                quit,
                show_logs,
            }
        }

        pub fn install(&self) {
            self.menu.init_for_nsapp();
            self.window_menu.set_as_windows_menu_for_nsapp();
            self.help_menu.set_as_help_menu_for_nsapp();
        }

        pub fn is_quit(&self, event: &MenuEvent) -> bool {
            event.id() == self.quit.id()
        }

        pub fn is_show_logs(&self, event: &MenuEvent) -> bool {
            event.id() == self.show_logs.id()
        }
    }
}
