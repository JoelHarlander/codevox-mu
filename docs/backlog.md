# Backlog

Draft v1, 11 Nov 2026. Still to refine with Joel.

## Done

- Choose the UI toolkit. Decided 11 Nov 2026: iced. See the decision log.
- CI skeleton: GitHub Actions for Linux, Windows, and macOS (Apple Silicon and Intel). The smoke run opens the window. `macos-13` was retired on 4 Dec 2025, so Intel is `macos-15-intel`. Windows Arm (`aarch64-pc-windows-msvc`) is a cross-compile build only. The x64 runner cannot execute it, so that job does not smoke-test.

## Now

- Agree the PM check-in cadence. Decision needed.
- RISK, for Joel to decide. Not settled. YouTube Music has no official public playback API. In-app Spotify playback libraries such as librespot are unofficial. Respect each service's terms.

## Next

- Spotify sign-in: library, playlists, and liked songs.
- Playback in-app with Premium, or via Spotify Connect. Free accounts fall back to YouTube Music. Waits on the RISK decision.
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
