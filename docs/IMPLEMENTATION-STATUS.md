# MetaMagic Implementation Status

This file describes the current development state. Features listed as implemented have been added to the codebase, but platform and format coverage still needs broader validation.

## Implemented

- Native Tauri 2 desktop shell with a React UI and Rust backend.
- Local audio and folder selection, recursive scanning, import progress, and a persistent SQLite library.
- Virtualized track list, album and artist views, playlist management, favorites, search, sorting, filters, and configurable columns.
- Completeness scoring for common metadata and artwork fields, with an incomplete-tags worklist.
- Tag reading and writing through Lofty, including ID3v2.2, ID3v2.3, and ID3v2.4 support. The exact fields available for reading or writing depend on the audio format.
- Extended tag editor for track/disc numbers and totals, compilation flag, composer, work/movement, BPM, grouping, comments, lyrics, and additional credits, dates, and identifiers.
- Embedded artwork management: browse pictures, change picture type, add and remove pictures, and export the front cover as a folder image. Artwork edits create a backup copy.
- Batch tag editor with a before/after preview and per-file results.
- Text manipulation for all or selected files, with preview, backup copies, and supported undo/redo history.
- File renaming from tags using `{artist}`, `{album}`, `{title}`, `{year}`, and `{track}` patterns. Slashes create folders. The app previews target paths and supports selecting individual files.
- Direct filename editor that keeps the audio extension and previews the resulting filename.
- Persistent backup copies and change history for supported tag edits and file renames, including undo and redo.
- MusicBrainz lookup for one selected track, with candidate details and overwrite controls.
- LRCLIB lyrics search, synchronized lyric playback view, and saving plain or synchronized lyrics as `.txt` or `.lrc` sidecar files. Normal lyrics can also be stored in the audio tag.
- Local BPM and musical-key estimates, confidence values, Camelot notation, and a beat-start phase estimate. This is not a full Engine DJ beatgrid.
- Exact duplicate detection with byte comparison and confirmed moves to the system trash.
- Local audio playback, transport controls, seeking, and volume controls. Codec support depends on the operating system's WebView.
- Technical file information, settings, keyboard shortcuts, and file management actions.

## Verification completed

- `npm run build` succeeds on the current macOS development machine.
- `cargo check` succeeds on the current macOS development machine.
- A previous macOS application compilation succeeded; DMG packaging could not complete because `hdiutil` failed in that environment.

These checks do not confirm Windows or Linux builds, nor do they guarantee correct round-trip behavior for every metadata format and field.

## Not implemented or not validated yet

- Engine DJ / Engine OS playlist export and synchronization. The current Engine screen is a placeholder; no undocumented Engine database is modified.
- Full beatgrid analysis, hot-cue/loop support, audio fingerprinting, and loudness normalization.
- Smart playlists and additional catalog providers or batch auto-tagging.
- Windows and Linux builds, installers, and real-device validation.
- Broad playback validation across all file formats and codecs.
- Comprehensive automated tests and format-specific read/write fixtures.

There are no demo tracks in the library by default. Users must scan their own files or folders.
