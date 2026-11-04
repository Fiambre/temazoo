//! Cookie capture through a WebView2 window, so the user logs in like in a normal browser.
//!
//! Runs in its own process (`temazoo --login` / `--refresh`) because it needs its own event loop.
//! The WebView keeps a persistent profile, so a hidden `--refresh` run behaves like the browser
//! revisiting YouTube Music: Google rotates the session tokens and we save the fresh cookie.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    window::WindowBuilder,
};
use wry::{WebContext, WebViewBuilder};

const LOGIN_URL: &str = "https://accounts.google.com/ServiceLogin?service=youtube&continue=https%3A%2F%2Fmusic.youtube.com%2F";
const MUSIC_URL: &str = "https://music.youtube.com/";

/// Exit codes of the child process.
pub const OK: i32 = 0;
pub const CLOSED: i32 = 1;
pub const NEEDS_LOGIN: i32 = 2;
pub const FAILED: i32 = 3;

fn profile_dir() -> PathBuf {
    let root = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let (new, old) = (root.join("Temazoo").join("webview"), root.join("ytplayer").join("webview"));
    // The app used to be called "ytplayer": move its WebView profile so the session is kept.
    if !new.exists() && old.exists() {
        let _ = std::fs::create_dir_all(new.parent().unwrap_or(&root));
        let _ = std::fs::rename(&old, &new);
    }
    if !new.exists() && old.exists() {
        return old;
    }
    new
}

/// `visible = true`: interactive login. `false`: silent refresh of an existing session.
pub fn run(visible: bool) -> i32 {
    let event_loop = EventLoop::new();
    let Ok(window) = WindowBuilder::new()
        .with_title("Temazoo - Iniciar sesión")
        .with_inner_size(LogicalSize::new(520.0, 720.0))
        .with_visible(visible)
        .build(&event_loop)
    else {
        return FAILED;
    };
    let mut ctx = WebContext::new(Some(profile_dir()));
    let url = if visible { LOGIN_URL } else { MUSIC_URL };
    let Ok(webview) = WebViewBuilder::new_with_web_context(&mut ctx).with_url(url).build(&window) else {
        return FAILED;
    };

    let start = Instant::now();
    let mut logged_since: Option<Instant> = None;
    // Hidden runs wait longer so Google has time to rotate the tokens.
    let settle = Duration::from_secs(if visible { 3 } else { 6 });

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(500));
        match event {
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => std::process::exit(CLOSED),
            Event::NewEvents(_) => {
                let on_music = webview.url().is_ok_and(|u| u.starts_with("https://music.youtube.com"));
                let cookies = webview.cookies_for_url(MUSIC_URL).unwrap_or_default();
                let has = |n: &str| cookies.iter().any(|c| c.name() == n);
                if on_music && has("SID") && has("__Secure-3PAPISID") {
                    let since = *logged_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= settle {
                        let header = cookies.iter().map(|c| format!("{}={}", c.name(), c.value())).collect::<Vec<_>>().join("; ");
                        let json = serde_json::json!({ "cookie": header, "x-goog-authuser": "0" });
                        let saved = serde_json::to_string_pretty(&json).ok().and_then(|s| std::fs::write("headers.json", s).ok());
                        std::process::exit(if saved.is_some() { OK } else { FAILED });
                    }
                } else {
                    logged_since = None;
                    if !visible && start.elapsed() > Duration::from_secs(25) {
                        std::process::exit(NEEDS_LOGIN);
                    }
                }
            }
            _ => {}
        }
    })
}
