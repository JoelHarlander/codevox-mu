# Decisions

Dated log for CodeVox mU. Dates are Sydney time.

## 11 Nov 2026 — Clean build

Decided. Clean build, not a fork of Sonora (GPL-3). MixMusic and MediaHarbor are references for reading only, never for copying code.

Joel's words: "Go with a clean build".

## 11 Nov 2026 — Public repo

Decided. The repo is public at <https://github.com/JoelHarlander/codevox-mu>.

Joel's words: "public".

## 11 Nov 2026 — Licence

Decided. MIT. Text is in [LICENSE](../LICENSE).

Joel's words: "Use MIT".

## 11 Nov 2026 — UI toolkit

Decided. The UI toolkit is [iced](https://iced.rs).

Joel's words: "Go with iced".

## 11 Nov 2026 — Playback via unofficial libraries, risk accepted

Decided. Spotify has no sanctioned native-playback API. The Web API plus Connect, or the Web Playback SDK, need Premium or a browser engine, so native playback uses librespot (unofficial). YouTube Music has no public API, so search, library, and streaming use community libraries (ytmusicapi-style and yt-dlp-style extraction), which go against YouTube's terms.

They may break when the services change. There is account and takedown risk. Keep each service behind a swappable backend trait so a library can be replaced. Never commit credentials. Use the Web API for Spotify metadata where it suffices. Check the licence of each chosen library for compatibility with MIT. Flag any GPL dependency, for example librespot's licence, before adopting it.

Joel's words: "I think we accept the risk use libraries done by other people as mentioned."

## 11 Nov 2026 — Weekly backlog check-in

Decided. The weekly backlog check-in is Saturdays around 5am Sydney time.

Joel's words: "Yes, set up the Monday 9am check-in", then "make it sat 5am".

## 11 Nov 2026 — Visual style follows CodeVox

Decided. CodeVox mU follows the CodeVox visual style. The tokens are in [design/tokens.json](../design/tokens.json). Headings are Space Mono. Body text is IBM Plex Mono.

Joel's words: "I think we follow code box styling" / "Vox" (dictated, meaning CodeVox).

## 11 Nov 2026 — Bot drives to a testable build and merges green PRs

Decided. The bot drives CodeVox mU through the backlog until there is a build Joel can install and test. It merges a PR itself once all 5 CI jobs pass. It still never pushes to main or force-pushes.

Joel's words: "i'll need you to drive it I mean", then "Yes, merge green PRs yourself".

## 11 Nov 2026 — YouTube Music search uses ytmapi-rs

Decided. Unauthenticated song search uses [ytmapi-rs](https://crates.io/crates/ytmapi-rs) 0.3.4, the maintained Rust client for YouTube Music's InnerTube API. crates.io lists the licence as MIT, which matches this project's MIT licence. It is not GPL. The repository is <https://github.com/nick42d/youtui>, last published 7 Nov 2026. No API key or account token is committed. The crate carries the public web client key. Playback, sign-in, and the library stay behind the service trait so this client can be replaced.

Transitive licences were checked from the resolved lockfile. ytmapi-rs and json-crawler are MIT. reqwest is MIT OR Apache-2.0. tokio is MIT. No dependency is GPL-only. Two crates also offer a copyleft alternative, and this project uses the permissive grant: r-efi is MIT OR Apache-2.0 OR LGPL-2.1-or-later, and self_cell is Apache-2.0 OR GPL-2.0-only.

## 11 Nov 2026 — Spotify via librespot, YouTube Music for everyone else

Decided. No Spotify developer app. Spotify sign-in and playback go through librespot (Premium required; unofficial, against Spotify's terms, can be blocked). Free users and anything else play from YouTube Music, including Spotify playlists matched to YouTube Music tracks.

Joel's words: "Let's do the last 2".

YouTube Music playback resolves a video id with [innertube-rs](https://crates.io/crates/innertube-rs) 0.10.1 (MIT). [rusty_ytdl](https://crates.io/crates/rusty_ytdl) 0.7.4 is MIT OR Apache-2.0, but its last release was August 2024 and its Android client URLs were rejected with HTTP 403. [rustypipe](https://crates.io/crates/rustypipe) is GPL-3.0 and is not a dependency. Audio is decoded with symphonia through [rodio](https://crates.io/crates/rodio) 0.23.0 (MIT OR Apache-2.0) and played with cpal (WASAPI, CoreAudio, ALSA). Symphonia is MPL-2.0, a file-level weak copyleft, not GPL. rodio's MIT and Apache decoders do not read the AAC in the MP4 container YouTube serves.
