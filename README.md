# CodeVox mU

CodeVox mU is a native desktop app that will put Spotify and YouTube Music in one player.
One search, one mixed queue, and mixed playlists.
It targets Windows, macOS, and Linux.
The licence is MIT. The UI toolkit is [iced](https://iced.rs). YouTube Music tracks play in the window. Spotify sign-in uses librespot. Premium accounts play Spotify in the window. Other accounts play the matching YouTube Music track.

See the [decision log](docs/decisions.md) and the [backlog](docs/backlog.md).

## Build and run

Install the current stable Rust toolchain from <https://rustup.rs>. Then:

```sh
cargo run --release
```

The window is titled "CodeVox mU". Sign in to Spotify from the control under the title. The browser login uses Spotify's desktop client. The refresh token is stored in the OS cache directory for CodeVox mU, not in this repo. Search asks YouTube Music, and Spotify once you are signed in, and lists one row per recording, tagged by source, in the CodeVox style. Both searches need a network. If one fails, the other rows stay and an error line is shown. A Premium account plays a Spotify row in the window. A free account, or a failed sign-in, plays the YouTube Music match for that title and artist. The bar says so.

```sh
cargo run --release -- --smoke
```

`--smoke` draws three frames and exits 0. Any other argument is a usage error and exits 2. Continuous integration uses `--smoke` to prove the binary opens a window.

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
`src/music.rs` is the service trait, the shared track and playlist types, the mixed queue, and the mock catalogues.
`src/search.rs` merges per-service search hits into one list and collapses the same recording.
`src/youtube_music.rs` is unauthenticated YouTube Music song search, and the Spotify fallback match.
`src/spotify.rs` is Spotify sign-in, search, and librespot playback.
`src/extract.rs` resolves a YouTube video id to an audio stream.
`src/player.rs` is play, pause, stop, next, and previous over the mixed queue.
`src/output.rs` decodes with symphonia and plays through cpal.
`src/media_controls.rs` is OS media keys and the system now-playing overlay.
`src/shell.rs` is the iced window.
A new concern gets its own module when it has real code. YouTube Music sign-in is not in the tree yet.
