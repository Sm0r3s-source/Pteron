# Pteron

A small, low-memory, general-purpose web browser for Windows, built on the [Servo](https://servo.org) engine (v0.7.0) in Rust. Current build: **52**.

Pteron is a young prototype. It aims to use little RAM and keep the interface simple: a sidebar with tabs, bookmarks and settings, and a page area that gets the rest of the window. Many modern sites work; some look wrong or do not work yet (see Known problems).

## Download

Get **Pteron-Setup.exe** from the [latest release](https://github.com/Sm0r3s-source/pteron/releases/latest). Windows may show a "protected your PC" warning because the installer is not signed yet: choose **More info**, then **Run anyway**.

Pteron updates itself: **Settings > About > Check for updates**. Updates are checked against a SHA-256 checksum before they are installed.

## What it does

- **Sidebar browsing**: tabs (drag to reorder, pin, sleep unused tabs to save memory), bookmarks with folders, downloads, find in page (Ctrl+F), reopen closed tab (Ctrl+Shift+T), page zoom with an on-screen percentage.
- **Address bar**: suggestions from bookmarks and history, and shortcuts such as `yt lofi`, `w rust`, `gh servo` (also `g`, `ddg`, `maps`, `r`).
- **Search engines**: DuckDuckGo, Google and others, or your own SearXNG.
- **Look**: color themes, accent colors, a custom theme from one color, new tab page backgrounds.
- **Privacy and safety**: ad and tracker blocking, a warning before opening plain http sites, optional history and cache clearing on exit.
- **Reading mode** (the Aa button) for articles.
- **Sync** of bookmarks and settings between computers through a folder you choose (for example a cloud-synced folder).
- **Bookmark import and export** (Chrome, Edge and Firefox files).
- **Plume**: a simple YouTube player and music view that plays videos up to 1080p60 (H.264).
- **Report a problem**: saves a small text file with versions, your settings and the newest log, to send to whoever gave you Pteron.

## Known problems

- Sites with protected (DRM) media do not work: Spotify, Netflix and similar.
- Sites with strict anti-bot checks may not let you in.
- Some pages look wrong or load slowly because Servo does not support every web feature yet.
- No browser extensions, no password manager yet, no private window.
- Windows only for now. The memory savings have not been measured against other browsers yet.

The full list is in the app (Settings > About > Known problems).

## Building

This repository holds Pteron's own changes as files laid out like the Servo source tree. To build: check out Servo v0.7.0, copy these folders over it, and run `./mach build -r` (Windows, with the Servo build requirements installed).

## License

Pteron's changes are released under the Mozilla Public License 2.0, the same license as Servo. Servo is a project of the Servo Project Developers.
