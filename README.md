# CodeVox mU

CodeVox mU is a native desktop app that will put Spotify and YouTube Music in one player.
One search, one mixed queue, and mixed playlists.
It targets Windows, macOS, and Linux.
The licence is MIT. The UI toolkit is [iced](https://iced.rs). Playback and accounts are still open.

See the [decision log](docs/decisions.md) and the [backlog](docs/backlog.md).

## Build and run

Install the current stable Rust toolchain from <https://rustup.rs>. Then:

```sh
cargo run --release
```

The window is titled "CodeVox mU". The search box queries the in-memory Spotify and YouTube Music mocks and lists matches tagged by source, in the CodeVox style.

```sh
cargo run --release -- --smoke
```

`--smoke` draws three frames and exits 0. Any other argument is a usage error and exits 2. Continuous integration uses `--smoke` to prove the binary opens a window.

Unsigned installers for Windows, macOS, and Linux are built by the Package workflow and uploaded as Actions artifacts. See [install testing](docs/install-testing.md).

Linux needs the iced system libraries. On Debian or Ubuntu:

```sh
sudo apt install pkg-config libxkbcommon-dev libxkbcommon-x11-dev \
  libwayland-dev libx11-dev libxcb1-dev libxcursor-dev libxi-dev \
  libxrandr-dev libfontconfig1-dev libvulkan1 mesa-vulkan-drivers
```

A display is required. The CI workflow runs the smoke test under Xvfb with lavapipe, Mesa's software Vulkan driver.

Windows needs the MSVC build tools. macOS needs the Xcode command line tools.

## Layout

`src/main.rs` parses the invocation.
`src/theme.rs` is the CodeVox visual style (`design/tokens.json`).
`src/music.rs` is the service trait, the shared track and playlist types, the mixed queue, and the mock catalogues.
`src/shell.rs` is the iced window.
A new concern gets its own module when it has real code. Playback, accounts, and the network are not in the tree yet.
