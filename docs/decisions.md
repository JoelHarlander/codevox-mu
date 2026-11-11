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
