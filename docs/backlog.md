# Backlog

Draft v1, 11 Nov 2026. Still to refine with Joel.

## Done

- Choose the UI toolkit. Decided 11 Nov 2026: iced. See the decision log.
- CI skeleton: GitHub Actions for Linux, Windows, and macOS (Apple Silicon and Intel). The smoke run opens the window. `macos-13` was retired on 4 Dec 2025, so Intel is `macos-15-intel`. Windows Arm (`aarch64-pc-windows-msvc`) is a cross-compile build only. The x64 runner cannot execute it, so that job does not smoke-test.
- RISK. Decided 11 Nov 2026: playback via unofficial libraries, risk accepted. See [Playback via unofficial libraries, risk accepted](decisions.md#11-nov-2026--playback-via-unofficial-libraries-risk-accepted).
- PM check-in cadence. Decided 11 Nov 2026: weekly backlog check-in, Saturdays around 5am Sydney time. See [Weekly backlog check-in](decisions.md#11-nov-2026--weekly-backlog-check-in).

## Now

- A services/backend trait plus a mock.

## Next

Each of the first three is its own small PR, in this order:

- Spotify sign-in (OAuth PKCE, Web API library): library, playlists, and liked songs.
- librespot playback spike. In-app playback; free accounts fall back to YouTube Music.
- YouTube Music search spike.
- YouTube Music sign-in.
- One search across both services.
- A mixed queue.
- Mixed playlists.
- Playlist import from either service.

## Later

- Installers for Windows 10/11 (x64 and Arm), macOS 12+ (universal), and Linux (AppImage, deb, rpm).
- GitHub Actions release builds for all three operating systems.
- Signing and notarisation.
- OS media keys and media controls.
- A tray or mini player.
- Synced lyrics.
- Local files.
