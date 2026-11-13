//! Tray icon for the menu bar, notification area, and Linux StatusNotifier.
//!
//! Windows uses the notification area. macOS uses the menu bar. Linux uses
//! StatusNotifier over the session bus (the ksni backend of tray-icon), not
//! GTK or AppIndicator. If Linux has no StatusNotifier host, or the icon
//! cannot be created, [`attach`] returns false and the caller quits on close
//! instead of hiding a window the listener cannot get back.
//!
//! `--smoke` and `--screenshot` never call [`attach`]. A headless run does
//! not need a tray host, and a failed icon does not panic.

use std::sync::{Mutex, OnceLock};

use iced::futures::channel::mpsc;
use iced::futures::{SinkExt, Stream, StreamExt};

/// What the tray menu or a click on the icon asked the window to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    PlayPause,
    Next,
    Previous,
    ToggleMini,
    ToggleWindow,
    /// Bring the window back. A click that finds it already visible does not hide it.
    Show,
    Quit,
}

/// The track and window state drawn on the icon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub title: String,
    pub artist: String,
    pub playing: bool,
    pub mini: bool,
    pub hidden: bool,
}

const ID_PLAY: &str = "codevox-mu.play";
const ID_PREVIOUS: &str = "codevox-mu.previous";
const ID_NEXT: &str = "codevox-mu.next";
const ID_MINI: &str = "codevox-mu.mini";
const ID_WINDOW: &str = "codevox-mu.window";
const ID_QUIT: &str = "codevox-mu.quit";

/// Menu rows stay on one line. A long title is cut here.
const TRACK_LABEL_LIMIT: usize = 80;

/// Status item size in pixels. The bundled icon is 512 and is scaled down.
const TRAY_EDGE: u32 = 64;

/// Menu id for a click, when the row is one this player handles.
pub fn action_from_id(id: &str) -> Option<Action> {
    match id {
        ID_PLAY => Some(Action::PlayPause),
        ID_PREVIOUS => Some(Action::Previous),
        ID_NEXT => Some(Action::Next),
        ID_MINI => Some(Action::ToggleMini),
        ID_WINDOW => Some(Action::ToggleWindow),
        ID_QUIT => Some(Action::Quit),
        _ => None,
    }
}

/// Icon clicks that should raise the window.
///
/// Left click on Linux is the StatusNotifier activate signal. The menu is
/// the right click there. Windows and macOS open the menu on left click, so
/// a left click is not also a show. A double click still raises the window.
pub fn action_from_icon(event: &tray_icon::TrayIconEvent) -> Option<Action> {
    match event {
        tray_icon::TrayIconEvent::DoubleClick {
            button: tray_icon::MouseButton::Left,
            ..
        } => Some(Action::Show),
        tray_icon::TrayIconEvent::Click {
            button: tray_icon::MouseButton::Left,
            button_state: tray_icon::MouseButtonState::Up,
            ..
        } => left_click_shows(),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn left_click_shows() -> Option<Action> {
    Some(Action::Show)
}

#[cfg(not(target_os = "linux"))]
fn left_click_shows() -> Option<Action> {
    None
}

/// Disabled menu row for the loaded track.
pub fn track_label(title: &str, artist: &str) -> String {
    let title = title.trim();
    let artist = artist.trim();
    let label = match (title.is_empty(), artist.is_empty()) {
        (true, true) => "Nothing playing".to_string(),
        (false, true) => title.to_string(),
        (true, false) => artist.to_string(),
        (false, false) => format!("{title} - {artist}"),
    };
    truncate_chars(&label, TRACK_LABEL_LIMIT)
}

/// Hover text. Idle is the product name. A track keeps that name in front.
pub fn tooltip(title: &str, artist: &str) -> String {
    if title.trim().is_empty() && artist.trim().is_empty() {
        "CodeVox mU".to_string()
    } else {
        format!("CodeVox mU - {}", track_label(title, artist))
    }
}

pub fn play_pause_label(playing: bool) -> &'static str {
    if playing { "Pause" } else { "Play" }
}

pub fn window_label(hidden: bool) -> &'static str {
    if hidden { "Show window" } else { "Hide window" }
}

/// Menu labels treat `&` as a mnemonic. A title keeps the character.
pub fn menu_text(text: &str) -> String {
    text.replace('&', "&&")
}

fn truncate_chars(label: &str, limit: usize) -> String {
    if label.chars().count() <= limit {
        return label.to_string();
    }
    let mut shortened: String = label.chars().take(limit.saturating_sub(1)).collect();
    shortened.push('…');
    shortened
}

/// Create the icon once. Later calls return the same answer.
///
/// Tests never create an icon. A panic inside the platform backend becomes
/// `false` so the window still runs.
pub fn attach() -> bool {
    static RESULT: OnceLock<bool> = OnceLock::new();
    *RESULT.get_or_init(attach_once)
}

fn attach_once() -> bool {
    #[cfg(test)]
    {
        false
    }
    #[cfg(not(test))]
    {
        attach_platform()
    }
}

/// Commands from the icon, once [`attach`] has stored a sender.
pub fn events() -> iced::Subscription<Action> {
    iced::Subscription::run(listen)
}

fn listen() -> impl Stream<Item = Action> {
    iced::stream::channel(16, async |mut output| {
        let (sender, mut receiver) = mpsc::channel(16);
        {
            let mut slot = emitters()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = Some(sender);
        }
        while let Some(action) = receiver.next().await {
            if output.send(action).await.is_err() {
                break;
            }
        }
    })
}

fn emitters() -> &'static Mutex<Option<mpsc::Sender<Action>>> {
    static EMITTERS: OnceLock<Mutex<Option<mpsc::Sender<Action>>>> = OnceLock::new();
    EMITTERS.get_or_init(|| Mutex::new(None))
}

/// Refresh the menu and the tooltip. No icon means this does nothing.
pub fn sync(snapshot: Snapshot) {
    SESSION.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(live) = slot.as_mut() else {
            return;
        };
        if live.shown.as_ref() == Some(&snapshot) {
            return;
        }
        live.track
            .set_text(menu_text(&track_label(&snapshot.title, &snapshot.artist)));
        live.play.set_text(play_pause_label(snapshot.playing));
        live.window.set_text(window_label(snapshot.hidden));
        live.mini.set_checked(snapshot.mini);
        let _ = live
            .icon
            .set_tooltip(Some(tooltip(&snapshot.title, &snapshot.artist)));
        live.shown = Some(snapshot);
    });
}

std::thread_local! {
    static SESSION: std::cell::RefCell<Option<Live>> = const { std::cell::RefCell::new(None) };
}

struct Live {
    icon: tray_icon::TrayIcon,
    track: tray_icon::menu::MenuItem,
    play: tray_icon::menu::MenuItem,
    mini: tray_icon::menu::CheckMenuItem,
    window: tray_icon::menu::MenuItem,
    shown: Option<Snapshot>,
}

#[cfg(not(test))]
fn attach_platform() -> bool {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(try_attach));
    match result {
        Ok(Ok(())) => {
            log::info!("tray: icon ready");
            eprintln!("codevox-mu: tray icon ready");
            true
        }
        Ok(Err(reason)) => {
            log::warn!("tray: unavailable: {reason}");
            eprintln!("codevox-mu: tray icon unavailable: {reason}");
            false
        }
        Err(_) => {
            log::warn!("tray: unavailable: startup panicked");
            eprintln!("codevox-mu: tray icon unavailable: startup panicked");
            false
        }
    }
}

#[cfg(not(test))]
fn try_attach() -> Result<(), String> {
    if !host_available() {
        return Err("no StatusNotifier host".to_string());
    }
    let (rgba, width, height) = tray_image()?;
    let icon =
        tray_icon::Icon::from_rgba(rgba, width, height).map_err(|error| error.to_string())?;
    let parts = menu_parts()?;
    let icon = tray_icon::TrayIconBuilder::new()
        .with_menu(Box::new(parts.menu))
        .with_tooltip("CodeVox mU")
        .with_icon(icon)
        .with_autosave_name("codevox-mu")
        .build()
        .map_err(|error| error.to_string())?;
    SESSION.with(|slot| {
        *slot.borrow_mut() = Some(Live {
            icon,
            track: parts.track,
            play: parts.play,
            mini: parts.mini,
            window: parts.window,
            shown: None,
        });
    });
    start_forwarder();
    Ok(())
}

#[cfg(not(test))]
struct MenuParts {
    menu: tray_icon::menu::Menu,
    track: tray_icon::menu::MenuItem,
    play: tray_icon::menu::MenuItem,
    mini: tray_icon::menu::CheckMenuItem,
    window: tray_icon::menu::MenuItem,
}

#[cfg(not(test))]
fn menu_parts() -> Result<MenuParts, String> {
    use tray_icon::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};

    let track = MenuItem::with_id("codevox-mu.track", "Nothing playing", false, None);
    let play = MenuItem::with_id(ID_PLAY, "Play", true, None);
    let previous = MenuItem::with_id(ID_PREVIOUS, "Previous", true, None);
    let next = MenuItem::with_id(ID_NEXT, "Next", true, None);
    let mini = CheckMenuItem::with_id(ID_MINI, "Mini player", true, false, None);
    let window = MenuItem::with_id(ID_WINDOW, "Hide window", true, None);
    let quit = MenuItem::with_id(ID_QUIT, "Quit", true, None);
    let gap_top = PredefinedMenuItem::separator();
    let gap_mid = PredefinedMenuItem::separator();
    let gap_bottom = PredefinedMenuItem::separator();
    let menu = Menu::new();
    for item in [
        &track as &dyn tray_icon::menu::IsMenuItem,
        &gap_top,
        &play,
        &previous,
        &next,
        &gap_mid,
        &mini,
        &window,
        &gap_bottom,
        &quit,
    ] {
        menu.append(item).map_err(|error| error.to_string())?;
    }
    Ok(MenuParts {
        menu,
        track,
        play,
        mini,
        window,
    })
}

#[cfg(not(test))]
fn host_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux_status_notifier_host()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// `false` when the session bus is missing or no host owns the watcher name.
///
/// A failure here is the graceful path: the player keeps running without an icon.
/// KDE, GNOME's AppIndicator extension, and Ayatana all register this name.
#[cfg(all(not(test), target_os = "linux"))]
fn linux_status_notifier_host() -> bool {
    const STATUS_NOTIFIER_WATCHER: &str = "org.kde.StatusNotifierWatcher";
    let Ok(connection) = dbus::blocking::Connection::new_session() else {
        return false;
    };
    let proxy = connection.with_proxy(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        std::time::Duration::from_millis(400),
    );
    let result: std::result::Result<(bool,), dbus::Error> = proxy.method_call(
        "org.freedesktop.DBus",
        "NameHasOwner",
        (STATUS_NOTIFIER_WATCHER,),
    );
    matches!(result, Ok((true,)))
}

#[cfg(not(test))]
fn start_forwarder() {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let started = std::thread::Builder::new()
        .name("tray-events".into())
        .spawn(forward_loop);
    if let Err(error) = started {
        log::warn!("tray: event thread did not start: {error}");
        eprintln!("codevox-mu: tray event thread did not start: {error}");
    }
}

#[cfg(not(test))]
fn forward_loop() {
    loop {
        let mut progressed = false;
        while let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
            progressed = true;
            if let Some(action) = action_from_id(event.id.as_ref()) {
                emit(action);
            }
        }
        while let Ok(event) = tray_icon::TrayIconEvent::receiver().try_recv() {
            progressed = true;
            if let Some(action) = action_from_icon(&event) {
                emit(action);
            }
        }
        if !progressed {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

#[cfg(not(test))]
fn emit(action: Action) {
    let mut slot = emitters()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(sender) = slot.as_mut() {
        let _ = sender.try_send(action);
    }
}

fn tray_image() -> Result<(Vec<u8>, u32, u32), String> {
    let (rgba, width, height) = decode_rgba(include_bytes!("../assets/icon.png"))
        .ok_or_else(|| "the tray icon could not be read".to_string())?;
    fit_square(&rgba, width, height, TRAY_EDGE)
        .ok_or_else(|| "the tray icon could not be scaled".to_string())
}

fn decode_rgba(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().ok()?;
    let mut buffer = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buffer).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let row = (info.width as usize).checked_mul(4)?;
    if info.line_size < row {
        return None;
    }
    let mut rgba = Vec::with_capacity(row.checked_mul(info.height as usize)?);
    for y in 0..info.height as usize {
        let start = y * info.line_size;
        rgba.extend_from_slice(buffer.get(start..start + row)?);
    }
    Some((rgba, info.width, info.height))
}

/// Average blocks of a packed RGBA image into a square of `edge` pixels.
fn fit_square(rgba: &[u8], width: u32, height: u32, edge: u32) -> Option<(Vec<u8>, u32, u32)> {
    if edge == 0 || width == 0 || height == 0 {
        return None;
    }
    let pixels = (width as usize).checked_mul(height as usize)?;
    let bytes = pixels.checked_mul(4)?;
    if rgba.len() < bytes {
        return None;
    }
    if width == edge && height == edge {
        return Some((rgba[..bytes].to_vec(), width, height));
    }
    let mut out = vec![0u8; (edge as usize).checked_mul(edge as usize)?.checked_mul(4)?];
    for y in 0..edge {
        let y0 = y * height / edge;
        let y1 = ((y + 1) * height / edge).max(y0 + 1).min(height);
        for x in 0..edge {
            let x0 = x * width / edge;
            let x1 = ((x + 1) * width / edge).max(x0 + 1).min(width);
            let mut acc = [0u32; 4];
            let mut count = 0u32;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let index = ((sy * width + sx) * 4) as usize;
                    let pixel = rgba.get(index..index + 4)?;
                    for (total, sample) in acc.iter_mut().zip(pixel) {
                        *total += u32::from(*sample);
                    }
                    count += 1;
                }
            }
            if count == 0 {
                return None;
            }
            let index = ((y * edge + x) * 4) as usize;
            let pixel = out.get_mut(index..index + 4)?;
            for (sample, total) in pixel.iter_mut().zip(acc) {
                *sample = u8::try_from(total / count).ok()?;
            }
        }
    }
    Some((out, edge, edge))
}

#[cfg(test)]
mod tests {
    use super::{
        Action, action_from_icon, fit_square, menu_text, play_pause_label, tooltip, track_label,
        tray_image, window_label,
    };

    #[test]
    fn menu_ids_map_to_player_actions() {
        assert_eq!(
            super::action_from_id(super::ID_PLAY),
            Some(Action::PlayPause)
        );
        assert_eq!(super::action_from_id(super::ID_NEXT), Some(Action::Next));
        assert_eq!(
            super::action_from_id(super::ID_PREVIOUS),
            Some(Action::Previous)
        );
        assert_eq!(
            super::action_from_id(super::ID_MINI),
            Some(Action::ToggleMini)
        );
        assert_eq!(
            super::action_from_id(super::ID_WINDOW),
            Some(Action::ToggleWindow)
        );
        assert_eq!(super::action_from_id(super::ID_QUIT), Some(Action::Quit));
        assert_eq!(super::action_from_id("codevox-mu.track"), None);
    }

    #[test]
    fn track_label_joins_title_and_artist_and_limits_the_length() {
        assert_eq!(track_label("  ", ""), "Nothing playing");
        assert_eq!(track_label("Night Drive", ""), "Night Drive");
        assert_eq!(track_label("", "Jonah Reed"), "Jonah Reed");
        assert_eq!(
            track_label("Night Drive", "Jonah Reed"),
            "Night Drive - Jonah Reed"
        );
        let long = "a".repeat(120);
        let label = track_label(&long, "");
        assert_eq!(label.chars().count(), super::TRACK_LABEL_LIMIT);
        assert!(label.ends_with('…'));
    }

    #[test]
    fn tooltip_names_the_product_and_the_track() {
        assert_eq!(tooltip("", "  "), "CodeVox mU");
        assert_eq!(
            tooltip("Night Drive", "Jonah Reed"),
            "CodeVox mU - Night Drive - Jonah Reed"
        );
    }

    #[test]
    fn play_and_window_labels_follow_the_state() {
        assert_eq!(play_pause_label(false), "Play");
        assert_eq!(play_pause_label(true), "Pause");
        assert_eq!(window_label(false), "Hide window");
        assert_eq!(window_label(true), "Show window");
    }

    #[test]
    fn menu_text_keeps_an_ampersand() {
        assert_eq!(menu_text("Rock & Roll"), "Rock && Roll");
    }

    #[test]
    fn fit_square_averages_a_block_and_rejects_a_short_buffer() {
        let rgba = vec![0, 0, 0, 255, 255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255];
        let (out, width, height) = fit_square(&rgba, 2, 2, 1).expect("one pixel");
        assert_eq!((width, height), (1, 1));
        assert_eq!(out.len(), 4);
        assert!(out[3] > 0);
        assert!(fit_square(&rgba[..8], 2, 2, 1).is_none());
        let (same, width, height) = fit_square(&rgba, 2, 2, 2).expect("already square");
        assert_eq!((width, height), (2, 2));
        assert_eq!(same, rgba);
    }

    #[test]
    fn bundled_icon_scales_to_the_tray_edge() {
        let (rgba, width, height) = tray_image().expect("icon");
        assert_eq!((width, height), (super::TRAY_EDGE, super::TRAY_EDGE));
        assert_eq!(
            rgba.len(),
            (super::TRAY_EDGE as usize) * (super::TRAY_EDGE as usize) * 4
        );
        assert!(rgba.chunks(4).any(|pixel| pixel[3] == 255));
    }

    #[test]
    fn a_left_click_shows_the_window_only_where_it_is_not_the_menu() {
        let click = tray_icon::TrayIconEvent::Click {
            id: tray_icon::TrayIconId::new("codevox-mu"),
            position: tray_icon::dpi::PhysicalPosition::new(0.0, 0.0),
            rect: tray_icon::Rect::default(),
            button: tray_icon::MouseButton::Left,
            button_state: tray_icon::MouseButtonState::Up,
        };
        #[cfg(target_os = "linux")]
        assert_eq!(action_from_icon(&click), Some(Action::Show));
        #[cfg(not(target_os = "linux"))]
        assert_eq!(action_from_icon(&click), None);

        let double = tray_icon::TrayIconEvent::DoubleClick {
            id: tray_icon::TrayIconId::new("codevox-mu"),
            position: tray_icon::dpi::PhysicalPosition::new(1.0, 1.0),
            rect: tray_icon::Rect::default(),
            button: tray_icon::MouseButton::Left,
        };
        assert_eq!(action_from_icon(&double), Some(Action::Show));
    }

    #[test]
    fn attach_from_a_test_does_not_create_an_icon() {
        assert!(!super::attach());
        assert!(!super::attach());
    }
}
