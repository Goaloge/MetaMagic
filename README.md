# MetaMagic

MetaMagic is a native desktop music library manager and tag editor built with **Tauri 2**, **React**, and **Rust**. It is designed for macOS, Linux, and Windows, with local-first library management and DJ-focused metadata tools.

> **Early development:** MetaMagic is not a stable release yet. macOS is the current development platform. Linux and Windows builds, Engine DJ export, and several planned audio features have not been completed or validated.

## Features

- Scan local audio files and folders into a persistent SQLite library.
- Browse tracks, albums, artists, playlists, favorites, and incomplete tags.
- Read and edit common metadata, including track/disc totals, compilation status, credits, identifiers, comments, and lyrics.
- Read ID3v2.2, ID3v2.3, and ID3v2.4 tags through Lofty.
- Browse, add, remove, and classify embedded artwork; export artwork as a folder image.
- Preview batch tag edits, text transformations, and tag-based filename changes before applying them.
- Keep file backups and use the change history to undo or redo supported tag edits and renames.
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
