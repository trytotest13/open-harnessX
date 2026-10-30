//! The one native desktop window (WebView2 via wry + tao): chat, models,
//! settings and the embedded Agent IDE all live inside it: nothing opens
//! extra windows or browser tabs.

use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

/// True when something answers on the fixed UI port with Open Harness's
/// status JSON, i.e. another Open Harness instance is already running.
pub(crate) fn ui_already_serving() -> bool {
    use std::io::{BufRead, BufReader, Write};
    let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", 3080)) else {
        return false;
    };
    s.set_read_timeout(Some(std::time::Duration::from_secs(2))).ok();
    if s.write_all(
        b"GET /api/status HTTP/1.1\r\nHost: 127.0.0.1:3080\r\nConnection: close\r\n\r\n",
    )
    .is_err()
    {
        return false;
    }
    let mut reader = BufReader::new(s);
    let mut line = String::new();
    reader.read_line(&mut line).is_ok() && line.contains(" 200")
}

/// Build the main window (hosting the Open Harness UI that `start_server`
/// serves) and run the event loop until it closes. Closing the window leaves
/// the engine running in the background.
pub fn open_app(
    title: &str,
    start_server: impl FnOnce() -> Result<u16, String>,
) -> Result<(), String> {
    let event_loop = EventLoopBuilder::<()>::with_user_event().build();
    // Own the UI port when it's free; when a previous instance already serves
    // it (double-click while running), reuse that instance and open a window
    // on it: never fall back to the browser for this case.
    let port = match start_server() {
        Ok(p) => p,
        Err(e) => {
            if ui_already_serving() {
                println!("[oh] stack already running on 3080, opening a window on it");
                3080
            } else {
                return Err(e);
            }
        }
    };
    let url = format!("http://127.0.0.1:{port}");

    let window = WindowBuilder::new()
        .with_title(title)
        .with_inner_size(LogicalSize::new(1440.0, 900.0))
        .with_min_inner_size(LogicalSize::new(960.0, 600.0))
        .build(&event_loop)
        .map_err(|e| format!("window: {e}"))?;

    // build() (not build_as_child) seeds the webview from the parent's full
    // client area and attaches the parent subclass that tracks WM_SIZE:
    // build_as_child starts at zero bounds unless seeded, leaving slivers.
    let webview = WebViewBuilder::new()
        .with_url(&url)
        .build(&window)
        .map_err(|e| format!("webview (is the WebView2 runtime installed?): {e}"))?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            Event::MainEventsCleared => window.request_redraw(),
            _ => {
                // The webview pumps its own platform events; keep it alive.
                let _ = &webview;
            }
        }
    })
}
