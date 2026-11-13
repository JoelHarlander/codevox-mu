# CodeVox mU

CodeVox mU is a native desktop app that will put Spotify and YouTube Music in one player.
One search, one mixed queue, and mixed playlists.
It targets Windows, macOS, and Linux.
The licence is MIT. The UI toolkit is [iced](https://iced.rs). YouTube Music tracks play in the window. Signing in with a browser cookie shows that account's library and playlists. Spotify sign-in uses librespot. Premium accounts play Spotify in the window. Other accounts play the matching YouTube Music track.

See the [decision log](docs/decisions.md) and the [backlog](docs/backlog.md).

## Build and run

Install the current stable Rust toolchain from <https://rustup.rs>. Then:

```sh
cargo run --release
```

The window is titled "CodeVox mU" and draws its own title bar, so the operating-system frame stays off. A sidebar switches Search, Queue, Library, and Accounts. Search asks YouTube Music, and Spotify once you are signed in, and lists one row per recording, tagged by source, in the CodeVox style. Both searches need a network. If one fails, the other rows stay and the error shows in a dismissible banner. The now-playing bar runs along the bottom of the window. A Premium account plays a Spotify row in the window. A free account, or a failed sign-in, plays the YouTube Music match for that title and artist. The bar says so.

Sign in from Accounts. Spotify opens the browser login for CodeVox mU's own Spotify app. A Premium account approves playback as well, which librespot signs in with Spotify's desktop client. Refresh tokens are stored in the OS cache directory for CodeVox mU, not in this repo. A saved sign-in that the app can no longer refresh asks to be renewed, and a rate limit is a short sentence rather than a status code. YouTube Music takes the Cookie header from music.youtube.com while that browser is signed in. The header has to include SAPISID. The cookie is stored in the OS keyring, or in the per-user config directory when the keyring is unavailable. The library lists playlists, with Liked songs first. Opening a playlist and clicking a track plays it the same way a search hit does. Sign out forgets the cookie. Search itself does not need an account. The main screen only shows whether each service is signed in.

A tray icon sits in the Windows notification area, the macOS menu bar, or a Linux StatusNotifier host. The menu shows the current track, play or pause, next, previous, the mini player, show or hide, and quit. Closing the window hides it there and leaves playback running. Quit is on that menu. If Linux has no StatusNotifier host, the icon is skipped and closing the window quits, so the process is not left with no window and no icon. The title bar's Mini button, and the same row in the tray menu, shrink the window to an always-on-top player with the title, the artist, and the transport controls. Full restores the previous size. Linux uses the session bus the media controls already use. There is no extra GTK or AppIndicator package.

```sh
cargo run --release -- --smoke
```

`--smoke` draws three frames and exits 0. `--screenshot <path.png>` writes that file, a sibling `<name>-accounts.png`, and a sibling `<name>-library.png`, then exits 0. Any other argument is a usage error and exits 2. Continuous integration uses `--smoke` to prove the binary opens a window.

Unsigned installers for Windows, macOS, and Linux are built by the Package workflow and uploaded as Actions artifacts. See [install testing](docs/install-testing.md).

Linux needs the iced system libraries, ALSA, and libdbus. On Debian or Ubuntu:

```sh
sudo apt install pkg-config libdbus-1-dev libasound2-dev libxkbcommon-dev libxkbcommon-x11-dev \
  libwayland-dev libx11-dev libxcb1-dev libxcursor-dev libxi-dev \
  libxrandr-dev libfontconfig1-dev libvulkan1 mesa-vulkan-drivers
```

A display is required. The CI workflow runs the smoke test under Xvfb with lavapipe, Mesa's software Vulkan driver.

Windows needs the MSVC build tools. macOS needs the Xcode command line tools.

## Layout

`src/main.rs` parses the invocation.
`src/theme.rs` is the CodeVox visual style (`design/tokens.json`).
`src/music.rs` is the service trait, the shared track and playlist types, and the mock catalogues.
`src/search.rs` merges per-service search hits into one list and collapses the same recording.
`src/youtube_music.rs` is unauthenticated YouTube Music song search, and the Spotify fallback match.
`src/youtube_account.rs` signs in with a browser cookie and reads the library and playlists.
`src/spotify.rs` is Spotify sign-in, search, and librespot playback.
`src/extract.rs` resolves a YouTube video id to an audio stream.
`src/queue.rs` is the mixed play order.
`src/playlists.rs` saves mixed playlists in the per-user data directory.
`src/player.rs` is play, pause, stop, next, and previous over the mixed queue.
`src/output.rs` decodes with symphonia and plays through cpal.
`src/media_controls.rs` is OS media keys and the system now-playing overlay.
`src/tray.rs` is the tray icon and its menu.
`src/shell.rs` is the iced window, including the mini player.
A new concern gets its own module when it has real code.
