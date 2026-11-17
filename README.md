# MetaMagic

**Current version: 0.2.0**

MetaMagic is a native desktop music library manager and tag editor built with **Tauri 2**, **React**, and **Rust**. It is designed for macOS, Linux, and Windows, with local-first library management and DJ-focused metadata tools.

> **Early development:** MetaMagic is not a stable release yet. macOS is the current development platform. Linux and Windows builds, Engine DJ export, and several planned audio features have not been completed or validated.

## Features

- Scan local audio files and folders into a persistent SQLite library.
- Browse tracks, albums, artists, playlists, favorites, and incomplete tags.
- Browse albums and artists grouped from metadata tags in either cover-tile or list views.
- Choose the number of tracks per table page or show all tracks in one virtualized list; sort table columns by clicking to switch between ascending and descending order, reorder columns by dragging, and adjust their widths.
- Read and edit common metadata, including track/disc totals, compilation status, credits, identifiers, comments, and lyrics.
- Read ID3v2.2, ID3v2.3, and ID3v2.4 tags through Lofty.
- Browse, add, remove, and classify embedded artwork; export artwork as a folder image.
- Search YouTube Music, check likely matches in the local library, and download selected results as tagged MP3s with embedded artwork. Mark multiple results and use the right-click menu for bulk selection. yt-dlp and Deno are managed in app data; FFmpeg is installed or updated through the platform package manager.
- Convert local video files to MP3 with FFmpeg and selectable bitrate. Audio is extracted from local files; search results are not downloaded.
- Preview batch tag edits, text transformations, and tag-based filename changes before applying them.
- Use the change history to undo or redo supported metadata edits and file renames. MetaMagic does not keep persistent audio-file backup copies.
- Search LRCLIB for lyrics and save plain or synchronized lyrics beside audio files.
- Analyze BPM and musical key locally, with confidence estimates and Camelot notation.
- Find exact duplicate files and move selected duplicates to the system trash after confirmation.
- Play local audio files using the desktop WebView's supported codecs.

Engine DJ / Engine OS integration is still a work in progress. The current Engine OS screen is a placeholder and does not export playlists or write Engine DJ library data.

## Requirements

- Node.js and npm
- Rust stable toolchain
- The Tauri prerequisites for your operating system (for example, Xcode Command Line Tools on macOS, WebView2 and Microsoft C++ Build Tools on Windows, or WebKitGTK on Linux)

## Run the desktop app

```sh
npm install
npm run desktop:dev
```

Build a desktop package for the current operating system with:

```sh
npm run desktop:build
```

The Vite browser server is for UI development only. Native file access, audio tagging, and library commands require the Tauri desktop host.

## Project status

See [Implementation status](docs/IMPLEMENTATION-STATUS.md) for the current feature inventory and [Implementation plan](docs/IMPLEMENTATION-PLAN.md) for the roadmap.

## Contributing

Contributions and issue reports are welcome. Please describe the operating system, audio format, and steps to reproduce any problem. Avoid uploading private music files or personal library data.

## Managed download tools

MetaMagic can download the official yt-dlp release binary and Deno into its application data directory. FFmpeg is installed or updated through Homebrew on macOS, winget on Windows, or supported Linux package managers. The app checks tool versions when the download panel opens. yt-dlp requires FFmpeg for audio extraction and a JavaScript runtime such as Deno for full YouTube support; see the [yt-dlp documentation](https://github.com/yt-dlp/yt-dlp#dependencies). The [FFmpeg download page](https://ffmpeg.org/download.html) links to platform-specific builds. Tool licensing follows the selected upstream build; yt-dlp's standalone release files and FFmpeg builds may have different license terms.

## What's new in 0.2.0

- Find album artwork through iTunes, Discogs, and MusicBrainz, then embed the selected image as the ID3 front cover.
- Search YouTube Music, match results against the local library, and download selected tracks with metadata and artwork.
- Install or update yt-dlp and Deno in the app; check and manage FFmpeg through the platform package manager.
- Convert local video files to MP3 with a selectable bitrate.
- Improve recovery and artwork loading for ID3-tagged MP3 files.
