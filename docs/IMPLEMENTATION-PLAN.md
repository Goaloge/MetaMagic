# MetaMagic Implementation Plan

## Goal

Build a reliable native music library manager for macOS, Linux, and Windows. Users should be able to inspect, correct, organize, and prepare local music collections with a clear preview before file changes.

## Architecture

- **Desktop:** Tauri 2 with a platform-neutral React interface and a native Rust backend.
- **Metadata:** A tested format/tag matrix built on Lofty. Do not claim support for a field or format combination before read/write round-trip checks pass.
- **Library:** SQLite indexes local files, metadata, analysis, playlists, and application state. Audio files remain the source of truth and the index must be rebuildable.
- **File safety:** Show target paths and changed values before batch operations. Keep backups, report per-file errors, and provide undo where reliable.
- **Online services:** Keep provider adapters separate, follow attribution and rate limits, and avoid putting service credentials in the UI.
- **Privacy:** Core library operations work locally without a server account. Clearly identify features that contact online services.

## Delivery phases

### Phase 0 — Product definition and technical validation

- Confirm target formats, metadata fields, operating systems, and user workflows.
- Evaluate the Tauri, Rust, database, tag, and audio libraries against primary documentation.
- Create a small fixture collection for supported formats and metadata edge cases.
- Record platform-specific prerequisites and limitations.

### Phase 1 — Native shell and library

- Build a Tauri desktop shell and native file/folder selection.
- Implement recursive scans with progress, cancellation, per-file errors, and large-library performance.
- Store indexed metadata and playlists in SQLite; support rescanning and index recovery.
- Implement search, filters, collections, favorites, and playlist organization.

### Phase 2 — Metadata and artwork

- Read and write supported tags, including Unicode, multiple values, and format-specific fields.
- Provide single-file and batch editing with before/after previews, backups, clear errors, and history.
- Support embedded artwork where the format permits it: display, add, replace, classify, remove, and export.
- Validate actual file round trips rather than assuming that a successful write preserves every field.

### Phase 3 — Batch operations and file organization

- Provide text transformations, tag-to-tag operations, and reusable action groups.
- Parse tags from filenames and create filenames/folders from metadata patterns.
- Preview all resulting names, folders, collisions, invalid characters, and selected targets before applying changes.
- Handle platform-specific path rules, case sensitivity, reserved names, and path lengths.

### Phase 4 — Online metadata and automatic tagging

- Add provider adapters for MusicBrainz, Apple iTunes, and Discogs, respecting each catalog's usage limits.
- Show candidate tracks, accurate release track mapping, source attribution, match scores, and proposed changes.
- Support review-first batch lookup and selective application for checked tracks.
- Add artwork search and selection with clear overwrite controls.
- Consider audio fingerprinting only after reviewing the provider's API terms, cost, privacy, and platform requirements.

### Phase 5 — Lyrics, player, and DJ analysis

- Search and preview plain or synchronized lyrics; embed them only where supported, otherwise save companion files.
- Provide local playback with transport controls, seeking, and volume controls; document codec differences across WebViews.
- Add and validate BPM/key analysis, loudness measurement/normalization, and duplicate detection.
- Improve DJ preparation with beatgrid and waveform analysis only when accuracy can be verified.

### Phase 6 — Denon Engine DJ / Engine OS workflow

- Support playlists and editable ordering in MetaMagic.
- Research official Engine DJ import and synchronization paths before designing export.
- Provide a USB/export preview with target drive checks, free-space checks, file copy plan, playlist order, and conflict report.
- Preserve hot cues, loops, and existing performance data only if a documented, testable workflow supports them.
- Validate exported media with Engine DJ Desktop and supported Engine OS hardware. Do not write undocumented proprietary database formats.

### Phase 7 — Export, diagnostics, and settings

- Export library data and artwork in documented formats.
- Add encoding diagnostics and preview-based repair for malformed metadata.
- Centralize settings for display, backups, sources, file access, and player behavior.
- Verify exports in common tools and confirm that Unicode survives round trips.

### Phase 8 — Quality and release

- Validate keyboard navigation, accessibility labels, contrast, and readable scaling.
- Test large libraries and all advertised metadata format/field combinations.
- Build, install, update, and uninstall packages on macOS, Windows, and Linux.
- Document supported formats, platform limitations, release notes, and known issues.

## Current priority

The core desktop library, tag editor, artwork management, text tools, renaming, lyrics lookup, player, and initial analysis have been implemented. The next major product phase is a documented and testable Engine DJ export workflow, alongside platform builds and format-specific verification.

## Explicitly out of scope for the first release

- Video-to-MP3 conversion, CD ripping, and CD burning, which require a separate codec and licensing review.
- A mobile companion app and phone synchronization.
- Reliance on a proprietary prebuilt metadata database.
