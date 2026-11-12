# Backlog

Ranked milestones to v1. Updated 12 Nov 2026. Every run starts from this list, top first.

**Done means working on a real screen on all 3 OSes.** That is Windows 11, macOS (Apple Silicon and Intel) and Linux, from the CI-built installer. Green CI plus a smoke run is only "Built". An item is "Done" once someone has used it on a real screen on each OS, and that person is named in the item.

Status: Done (real screen, 3 OSes) · Built (merged, CI green on 3 OSes, not yet used on a real screen) · In progress · Not started.

## Milestones to v1

| # | Milestone | Status |
|---|-----------|--------|
| 1 | YouTube Music search and playback: search, click a result, it plays, with pause, resume and next | Built (PR #8, #9) |
| 2 | Installers: Windows x64 and Arm (NSIS, zip), macOS universal dmg, Linux AppImage, deb and rpm, all unsigned, from CI | Built (PR #7) |
| 3 | Spotify sign-in and playback via librespot (Premium), with free accounts falling back to the matched YouTube Music track | Built (PR #12, YouTube Music fallback included) |
| 4 | One search across both services: merged, de-duplicated results tagged by source | Built (PR #14) |
| 5 | Mixed queue: add, reorder and remove tracks from either service, with next and previous across sources | Built |
| 6 | OS media keys and media controls: Windows SMTC, macOS Now Playing, Linux MPRIS | Built (PR #11) |
| 7 | Spotify library: playlists and liked songs | Built |
| 8 | YouTube Music sign-in: library and playlists | Built (PR #15, cookie sign-in; first page of each playlist only) |
| 9 | Mixed playlists: create, save locally, and play tracks from both services | In progress (PR #24) |
| 10 | Playlist import from Spotify or YouTube Music into a mixed playlist | Not started |
| 11 | Tray icon or mini player | Not started |
| 12 | Synced lyrics | Not started |
| 13 | Local files | Not started |
| 14 | Release builds on GitHub Actions for all 3 OSes (artifacts only, no publishing) | Built (package workflow) |

The ranking puts the playable core (1 to 5) first, so there's something real to test, then system integration (6), then libraries and playlists (7 to 10), then extras (11 to 13).

## Decided

- UI toolkit: iced (11 Nov 2026).
- RISK: playback via unofficial libraries, risk accepted (11 Nov 2026). See [the decision log](decisions.md#11-nov-2026--playback-via-unofficial-libraries-risk-accepted).
- Weekly check-in on Saturdays around 5am Sydney time (11 Nov 2026).
- Visual style follows CodeVox (11 Nov 2026).
- Spotify via librespot, with YouTube Music for everyone else (11 Nov 2026).

## After v1

- Signing and notarisation (needs paid certificates, so it needs Joel's yes).
- Public release: revisit Spotify access because of the librespot block risk.
