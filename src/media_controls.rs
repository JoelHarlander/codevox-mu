//! OS media keys and the system now-playing overlay.
//!
//! Windows uses System Media Transport Controls. macOS uses
//! `MPNowPlayingInfoCenter` and `MPRemoteCommandCenter`. Linux uses MPRIS
//! on the session bus. All three go through [`souvlaki`], which is MIT.
//!
//! The session lives on the UI thread. `--smoke` never calls [`attach`],
//! so a headless run does not need a D-Bus daemon. If the session cannot
//! be created, the failure is logged and playback continues.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::music::Source;
use crate::player::{NowPlaying, Phase, Playback};
use iced::futures::channel::mpsc;
use iced::futures::{SinkExt, Stream, StreamExt};

/// What a hardware key or the OS overlay asked the player to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaCommand {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
}

/// The player call for a [`MediaCommand`] in the current [`Phase`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Play,
    Pause,
    Next,
    Previous,
}

/// Title, artist, artwork, and source shown on the OS overlay.
///
/// Souvlaki has no source field. [`OverlayMetadata::source_line`] is the
/// album line, which is where the overlay shows a third string.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OverlayMetadata {
    pub title: String,
    pub artist: String,
    pub artwork_url: Option<String>,
    pub source: Option<Source>,
}

const DBUS_NAME: &str = "codevox_mu";
const DISPLAY_NAME: &str = "CodeVox mU";

static ATTACHED: AtomicBool = AtomicBool::new(false);
static QUIET: AtomicBool = AtomicBool::new(false);

struct Live {
    controls: souvlaki::MediaControls,
    shown: OverlayMetadata,
    phase: Phase,
    have_phase: bool,
}

thread_local! {
    static SESSION: std::cell::RefCell<Option<Live>> = const { std::cell::RefCell::new(None) };
}

fn emitters() -> &'static Mutex<Option<mpsc::Sender<MediaCommand>>> {
    static EMITTERS: OnceLock<Mutex<Option<mpsc::Sender<MediaCommand>>>> = OnceLock::new();
    EMITTERS.get_or_init(|| Mutex::new(None))
}

/// Play, pause, and skip map straight through. Toggle pauses only while
/// audio is actually playing. Loading still has nothing to pause.
pub fn transport_for(command: MediaCommand, phase: Phase) -> Transport {
    match command {
        MediaCommand::Play => Transport::Play,
        MediaCommand::Pause => Transport::Pause,
        MediaCommand::Next => Transport::Next,
        MediaCommand::Previous => Transport::Previous,
        MediaCommand::Toggle => match phase {
            Phase::Playing => Transport::Pause,
            Phase::Idle | Phase::Loading | Phase::Paused | Phase::Stopped => Transport::Play,
        },
    }
}

/// Hardware keys the player understands. Seek, stop, and window commands
/// are ignored.
pub fn command_from_platform(event: &souvlaki::MediaControlEvent) -> Option<MediaCommand> {
    match event {
        souvlaki::MediaControlEvent::Play => Some(MediaCommand::Play),
        souvlaki::MediaControlEvent::Pause => Some(MediaCommand::Pause),
        souvlaki::MediaControlEvent::Toggle => Some(MediaCommand::Toggle),
        souvlaki::MediaControlEvent::Next => Some(MediaCommand::Next),
        souvlaki::MediaControlEvent::Previous => Some(MediaCommand::Previous),
        souvlaki::MediaControlEvent::Stop
        | souvlaki::MediaControlEvent::Seek(_)
        | souvlaki::MediaControlEvent::SeekBy(_, _)
        | souvlaki::MediaControlEvent::SetPosition(_)
        | souvlaki::MediaControlEvent::SetVolume(_)
        | souvlaki::MediaControlEvent::OpenUri(_)
        | souvlaki::MediaControlEvent::Raise
        | souvlaki::MediaControlEvent::Quit => None,
    }
}

pub fn drive(playback: &Playback, command: MediaCommand) {
    match transport_for(command, playback.phase()) {
        Transport::Play => playback.play(),
        Transport::Pause => playback.pause(),
        Transport::Next => playback.next(),
        Transport::Previous => playback.previous(),
    }
}

impl OverlayMetadata {
    pub fn from_now(now: &NowPlaying) -> Self {
        if now.title.is_empty() && now.source.is_none() {
            return Self::default();
        }
        Self {
            title: now.title.clone(),
            artist: now.artist.clone(),
            artwork_url: artwork_url(now.source, &now.track_id),
            source: now.source,
        }
    }

    /// The third line of the OS overlay.
    pub fn source_line(&self) -> Option<String> {
        self.source.map(|source| source.label().to_string())
    }
}

/// YouTube serves a still at a stable URL for a video id. Spotify and
/// local tracks have no artwork URL in the player yet.
pub fn artwork_url(source: Option<Source>, track_id: &str) -> Option<String> {
    if source != Some(Source::YouTubeMusic) {
        return None;
    }
    if track_id.is_empty()
        || !track_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }
    Some(format!("https://i.ytimg.com/vi/{track_id}/hqdefault.jpg"))
}

/// Commands from the OS, once [`attach`] has stored a sender.
pub fn events() -> iced::Subscription<MediaCommand> {
    iced::Subscription::run(listen)
}

fn listen() -> impl Stream<Item = MediaCommand> {
    iced::stream::channel(16, async |mut output| {
        let (sender, mut receiver) = mpsc::channel(16);
        {
            let mut slot = emitters()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = Some(sender);
        }
        while let Some(command) = receiver.next().await {
            if output.send(command).await.is_err() {
                break;
            }
        }
    })
}

/// Read a Windows `HWND` from the iced window. Other platforms get `None`.
pub fn read_handle(id: iced::window::Id) -> iced::Task<Option<isize>> {
    iced::window::run(id, hwnd_of)
}

fn hwnd_of(window: &dyn iced::window::Window) -> Option<isize> {
    let handle = window.window_handle().ok()?;
    match handle.as_raw() {
        iced::window::raw_window_handle::RawWindowHandle::Win32(raw) => Some(raw.hwnd.get()),
        _ => None,
    }
}

/// Start the OS session. A second call does nothing. On Windows, a missing
/// window handle is logged and the session stays off. Any other failure is
/// logged and playback keeps going.
pub fn attach(hwnd: Option<isize>) {
    if ATTACHED.load(Ordering::SeqCst) {
        return;
    }
    #[cfg(target_os = "windows")]
    if hwnd.is_none() {
        ATTACHED.store(true, Ordering::SeqCst);
        eprintln!("codevox-mu: media controls unavailable: no window handle");
        return;
    }

    match connect(hwnd) {
        Ok(controls) => {
            ATTACHED.store(true, Ordering::SeqCst);
            SESSION.with(|slot| {
                *slot.borrow_mut() = Some(Live {
                    controls,
                    shown: OverlayMetadata::default(),
                    phase: Phase::Idle,
                    have_phase: false,
                });
            });
        }
        Err(error) => {
            ATTACHED.store(true, Ordering::SeqCst);
            eprintln!("codevox-mu: media controls unavailable: {error}");
        }
    }
}

/// Push the snapshot to the overlay. No session means this does nothing.
pub fn publish(now: &NowPlaying) {
    if !ATTACHED.load(Ordering::Relaxed) || QUIET.load(Ordering::Relaxed) {
        return;
    }
    let overlay = OverlayMetadata::from_now(now);
    let result = SESSION.with(|slot| -> Result<(), String> {
        let mut guard = slot.borrow_mut();
        let Some(live) = guard.as_mut() else {
            return Ok(());
        };
        if live.shown != overlay {
            push_metadata(&mut live.controls, now, &overlay)?;
            live.shown = overlay;
        }
        let moving = matches!(now.phase, Phase::Playing | Phase::Loading);
        if !live.have_phase || live.phase != now.phase || moving {
            push_playback(&mut live.controls, now)?;
            live.phase = now.phase;
            live.have_phase = true;
        }
        Ok(())
    });
    if let Err(error) = result
        && !QUIET.swap(true, Ordering::Relaxed)
    {
        eprintln!("codevox-mu: media controls update failed: {error}");
    }
}

fn connect(hwnd: Option<isize>) -> Result<souvlaki::MediaControls, String> {
    let mut controls = souvlaki::MediaControls::new(souvlaki::PlatformConfig {
        dbus_name: DBUS_NAME,
        display_name: DISPLAY_NAME,
        hwnd: hwnd_pointer(hwnd),
    })
    .map_err(|error| error.to_string())?;

    controls
        .attach(|event| {
            if let Some(command) = command_from_platform(&event) {
                emit(command);
            }
        })
        .map_err(|error| error.to_string())?;
    Ok(controls)
}

fn hwnd_pointer(hwnd: Option<isize>) -> Option<*mut std::ffi::c_void> {
    hwnd.map(|value| std::ptr::with_exposed_provenance_mut(value as usize))
}

fn emit(command: MediaCommand) {
    let guard = emitters()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(sender) = guard.as_ref() {
        let _ = sender.clone().try_send(command);
    }
}

fn push_metadata(
    controls: &mut souvlaki::MediaControls,
    now: &NowPlaying,
    overlay: &OverlayMetadata,
) -> Result<(), String> {
    let album = overlay.source_line();
    controls
        .set_metadata(souvlaki::MediaMetadata {
            title: nonempty(&overlay.title),
            artist: nonempty(&overlay.artist),
            album: album.as_deref(),
            cover_url: overlay.artwork_url.as_deref(),
            duration: (now.duration > std::time::Duration::ZERO).then_some(now.duration),
        })
        .map_err(|error| error.to_string())
}

fn push_playback(controls: &mut souvlaki::MediaControls, now: &NowPlaying) -> Result<(), String> {
    let progress = Some(souvlaki::MediaPosition(now.elapsed));
    let playback = match now.phase {
        Phase::Playing | Phase::Loading => souvlaki::MediaPlayback::Playing { progress },
        Phase::Paused => souvlaki::MediaPlayback::Paused { progress },
        Phase::Idle | Phase::Stopped => souvlaki::MediaPlayback::Stopped,
    };
    controls
        .set_playback(playback)
        .map_err(|error| error.to_string())
}

fn nonempty(value: &str) -> Option<&str> {
    if value.is_empty() { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn now_playing(
        title: &str,
        source: Option<Source>,
        track_id: &str,
        phase: Phase,
    ) -> NowPlaying {
        NowPlaying {
            title: title.to_string(),
            artist: "Ada Vega".to_string(),
            phase,
            elapsed: Duration::from_secs(3),
            duration: Duration::from_secs(180),
            error: None,
            note: None,
            can_play: phase != Phase::Playing,
            can_pause: phase == Phase::Playing,
            can_stop: matches!(phase, Phase::Playing | Phase::Paused),
            can_next: true,
            can_previous: false,
            source,
            track_id: track_id.to_string(),
            upcoming: Vec::new(),
            devices: Vec::new(),
            volume: None,
        }
    }

    #[test]
    fn direct_commands_keep_their_transport() {
        for phase in [
            Phase::Idle,
            Phase::Loading,
            Phase::Playing,
            Phase::Paused,
            Phase::Stopped,
        ] {
            assert_eq!(
                transport_for(MediaCommand::Play, phase),
                Transport::Play,
                "{phase:?}"
            );
            assert_eq!(
                transport_for(MediaCommand::Pause, phase),
                Transport::Pause,
                "{phase:?}"
            );
            assert_eq!(
                transport_for(MediaCommand::Next, phase),
                Transport::Next,
                "{phase:?}"
            );
            assert_eq!(
                transport_for(MediaCommand::Previous, phase),
                Transport::Previous,
                "{phase:?}"
            );
        }
    }

    #[test]
    fn toggle_pauses_only_while_playing() {
        assert_eq!(
            transport_for(MediaCommand::Toggle, Phase::Playing),
            Transport::Pause
        );
        for phase in [Phase::Idle, Phase::Loading, Phase::Paused, Phase::Stopped] {
            assert_eq!(
                transport_for(MediaCommand::Toggle, phase),
                Transport::Play,
                "{phase:?}"
            );
        }
    }

    #[test]
    fn platform_events_map_to_player_commands() {
        use souvlaki::{MediaControlEvent, MediaPosition, SeekDirection};

        assert_eq!(
            command_from_platform(&MediaControlEvent::Play),
            Some(MediaCommand::Play)
        );
        assert_eq!(
            command_from_platform(&MediaControlEvent::Pause),
            Some(MediaCommand::Pause)
        );
        assert_eq!(
            command_from_platform(&MediaControlEvent::Toggle),
            Some(MediaCommand::Toggle)
        );
        assert_eq!(
            command_from_platform(&MediaControlEvent::Next),
            Some(MediaCommand::Next)
        );
        assert_eq!(
            command_from_platform(&MediaControlEvent::Previous),
            Some(MediaCommand::Previous)
        );
        assert_eq!(command_from_platform(&MediaControlEvent::Stop), None);
        assert_eq!(
            command_from_platform(&MediaControlEvent::Seek(SeekDirection::Forward)),
            None
        );
        assert_eq!(
            command_from_platform(&MediaControlEvent::SeekBy(
                SeekDirection::Backward,
                Duration::from_secs(5),
            )),
            None
        );
        assert_eq!(
            command_from_platform(&MediaControlEvent::SetPosition(MediaPosition(
                Duration::from_secs(1)
            ))),
            None
        );
        assert_eq!(
            command_from_platform(&MediaControlEvent::SetVolume(0.5)),
            None
        );
        assert_eq!(
            command_from_platform(&MediaControlEvent::OpenUri("file://track".into())),
            None
        );
        assert_eq!(command_from_platform(&MediaControlEvent::Raise), None);
        assert_eq!(command_from_platform(&MediaControlEvent::Quit), None);
    }

    #[test]
    fn youtube_overlay_shows_title_artist_artwork_and_source() {
        let overlay = OverlayMetadata::from_now(&now_playing(
            "Night Drive",
            Some(Source::YouTubeMusic),
            "dQw4w9WgXcQ",
            Phase::Playing,
        ));

        assert_eq!(overlay.title, "Night Drive");
        assert_eq!(overlay.artist, "Ada Vega");
        assert_eq!(overlay.source, Some(Source::YouTubeMusic));
        assert_eq!(
            overlay.artwork_url.as_deref(),
            Some("https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg")
        );
        assert_eq!(overlay.source_line().as_deref(), Some("YouTube Music"));
    }

    #[test]
    fn spotify_overlay_has_no_artwork_url() {
        let overlay = OverlayMetadata::from_now(&now_playing(
            "Paper Boats",
            Some(Source::Spotify),
            "sp-paper",
            Phase::Paused,
        ));

        assert_eq!(overlay.source, Some(Source::Spotify));
        assert_eq!(overlay.artwork_url, None);
        assert_eq!(overlay.source_line().as_deref(), Some("Spotify"));
    }

    #[test]
    fn idle_overlay_is_empty() {
        let overlay = OverlayMetadata::from_now(&NowPlaying::default());
        assert_eq!(overlay, OverlayMetadata::default());
        assert_eq!(artwork_url(None, ""), None);
        assert_eq!(artwork_url(Some(Source::YouTubeMusic), "bad id"), None);
        assert_eq!(artwork_url(Some(Source::Local), "song"), None);
    }

    #[test]
    fn dormant_playback_accepts_a_media_command() {
        let playback = Playback::dormant();
        drive(&playback, MediaCommand::Toggle);
        drive(&playback, MediaCommand::Previous);
        assert_eq!(playback.phase(), Phase::Idle);
    }

    /// libdbus connects in `attach`. A dead socket must come back as an
    /// error, not a panic, and must not stop the player.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_session_bus_does_not_panic() {
        let previous = std::env::var_os("DBUS_SESSION_BUS_ADDRESS");
        // SAFETY: this is the only test that reads the session bus, and it
        // restores the previous value before returning. `set_var` is unsafe
        // because a concurrent caller could be reading the environment.
        unsafe {
            std::env::set_var(
                "DBUS_SESSION_BUS_ADDRESS",
                "unix:path=/tmp/codevox-mu-no-such-bus",
            );
        }
        attach(None);
        publish(&NowPlaying::default());
        // SAFETY: same as above. The restore happens before the test returns.
        unsafe {
            match previous {
                Some(value) => std::env::set_var("DBUS_SESSION_BUS_ADDRESS", value),
                None => std::env::remove_var("DBUS_SESSION_BUS_ADDRESS"),
            }
        }
    }
}
