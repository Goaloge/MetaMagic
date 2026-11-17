use lofty::config::{ParseOptions, ParsingMode, WriteOptions};
use lofty::file::{AudioFile, FileType, TaggedFile, TaggedFileExt};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, Tag};
use rusqlite::{params, Connection, OptionalExtension};
use rustfft::num_complex::Complex;
use rustfft::FftPlanner;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeMap;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tauri::ipc::Channel;
use tauri::Manager;
use zip::ZipArchive;
use walkdir::WalkDir;

fn relaxed_parse_options() -> ParseOptions { ParseOptions::new().parsing_mode(ParsingMode::Relaxed) }
fn best_attempt_parse_options() -> ParseOptions { ParseOptions::new().parsing_mode(ParsingMode::BestAttempt) }

fn read_tagged(path: &Path) -> Result<TaggedFile, String> {
    let first = Probe::open(path).map_err(|e| e.to_string())?.options(best_attempt_parse_options()).read();
    match first {
        Ok(tagged) => Ok(tagged),
        Err(error) if error.to_string().contains("UTF-16 string has an invalid byte order mark") => {
            let mut bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            repair_id3_utf16_frames(&mut bytes);
            let file_type = FileType::from_path(path).unwrap_or(FileType::Mpeg);
            if let Ok(tagged) = Probe::with_file_type(Cursor::new(bytes.clone()), file_type).options(relaxed_parse_options()).read() { return Ok(tagged); }
            if file_type == FileType::Mpeg {
                let mut audio = bytes;
                if strip_leading_id3v2(&mut audio) {
                    return Probe::with_file_type(Cursor::new(audio), FileType::Mpeg).options(relaxed_parse_options()).read().map_err(|_| error.to_string());
                }
            }
            Err(error.to_string())
        },
        Err(error) => Err(error.to_string()),
    }
}

fn strip_leading_id3v2(bytes: &mut Vec<u8>) -> bool {
    if bytes.len() < 10 || &bytes[..3] != b"ID3" { return false; }
    let size = bytes[6..10].iter().fold(0usize, |acc, byte| (acc << 7) | usize::from(byte & 0x7f));
    if bytes[6..10].iter().any(|byte| byte & 0x80 != 0) { return false; }
    let total = 10usize.saturating_add(size).saturating_add(if bytes[5] & 0x10 != 0 { 10 } else { 0 });
    if total >= bytes.len() { return false; }
    bytes.drain(..total);
    true
}

fn id3_picture_frames(bytes: &[u8]) -> Vec<(PictureType, String, Vec<u8>)> {
    if bytes.len() < 10 || &bytes[..3] != b"ID3" { return Vec::new(); }
    let version = bytes[3];
    if !(2..=4).contains(&version) { return Vec::new(); }
    let tag_size = bytes[6..10].iter().fold(0usize, |acc, byte| (acc << 7) | usize::from(byte & 0x7f));
    let limit = 10usize.saturating_add(tag_size).min(bytes.len());
    let mut position = 10usize;
    if version >= 3 && bytes[5] & 0x40 != 0 {
        if position + 4 > limit { return Vec::new(); }
        let ext = if version == 3 { u32::from_be_bytes(bytes[position..position+4].try_into().unwrap()) as usize } else { bytes[position..position+4].iter().fold(0usize, |acc, byte| (acc << 7) | usize::from(byte & 0x7f)) };
        position = position.saturating_add(if version == 3 { ext + 4 } else { ext });
    }
    let mut pictures = Vec::new();
    while position < limit {
        let header = if version == 2 { 6 } else { 10 };
        if position + header > limit { break; }
        let id_len = if version == 2 { 3 } else { 4 };
        if bytes[position..position+id_len].iter().all(|byte| *byte == 0) { break; }
        let id = &bytes[position..position+id_len];
        let frame_size = if version == 2 { (usize::from(bytes[position+3])<<16)|(usize::from(bytes[position+4])<<8)|usize::from(bytes[position+5]) } else if version == 4 { bytes[position+4..position+8].iter().fold(0usize, |acc, byte| (acc << 7) | usize::from(byte & 0x7f)) } else { u32::from_be_bytes(bytes[position+4..position+8].try_into().unwrap()) as usize };
        let start = position + header;
        let end = start.saturating_add(frame_size);
        if frame_size == 0 || end > limit { break; }
        if (version == 2 && id == b"PIC") || (version >= 3 && id == b"APIC") {
            let data = &bytes[start..end];
            if data.len() > 5 {
                let encoding = data[0];
                let (mime, mut cursor) = if version == 2 {
                    let fmt = String::from_utf8_lossy(data.get(1..4).unwrap_or_default()).to_ascii_lowercase();
                    (match fmt.as_str() { "png" => "image/png", "jpg"|"jpeg" => "image/jpeg", _ => "image/jpeg" }.to_owned(), 4usize)
                } else {
                    let mime_end = data[1..].iter().position(|byte| *byte == 0).map(|i| i+1).unwrap_or(1);
                    (String::from_utf8_lossy(&data[1..mime_end]).into_owned(), mime_end+1)
                };
                let picture_type = data.get(cursor).copied().unwrap_or(0); cursor += 1;
                if encoding == 1 || encoding == 2 {
                    while cursor + 1 < data.len() { if data[cursor] == 0 && data[cursor+1] == 0 { cursor += 2; break; } cursor += 2; }
                } else if let Some(end) = data.get(cursor..).and_then(|tail| tail.iter().position(|byte| *byte == 0)) { cursor += end + 1; }
                if cursor < data.len() {
                    let unsynchronized = bytes[5] & 0x80 != 0 || (version == 4 && bytes[position+9] & 0x02 != 0);
                    let encoded = if unsynchronized { remove_id3_unsynchronization(&data[cursor..]) } else { data[cursor..].to_vec() };
                    if let Some(image) = normalize_embedded_image(&encoded) { pictures.push((if picture_type == 3 { PictureType::CoverFront } else { PictureType::Other }, mime, image)); }
                }
            }
        }
        position = end;
    }
    pictures
}

fn remove_id3_picture_frame(bytes: &mut Vec<u8>, picture_index: usize) -> Result<(), String> {
    if bytes.len() < 10 || &bytes[..3] != b"ID3" { return Err("ID3v2-Tag nicht gefunden".into()); }
    let version = bytes[3];
    if !(2..=4).contains(&version) { return Err(format!("ID3v2.{version} wird für direktes Bildbearbeiten nicht unterstützt")); }
    let tag_size = bytes[6..10].iter().fold(0usize, |acc, byte| (acc << 7) | usize::from(byte & 0x7f));
    let limit = 10usize.saturating_add(tag_size).min(bytes.len());
    let mut position = 10usize;
    if version >= 3 && bytes[5] & 0x40 != 0 {
        if position + 4 > limit { return Err("ID3v2-Erweiterung ist beschädigt".into()); }
        let ext = if version == 3 { u32::from_be_bytes(bytes[position..position+4].try_into().unwrap()) as usize } else { bytes[position..position+4].iter().fold(0usize, |acc, byte| (acc << 7) | usize::from(byte & 0x7f)) };
        position = position.saturating_add(if version == 3 { ext + 4 } else { ext });
    }
    let mut pictures = Vec::new();
    while position < limit {
        let header = if version == 2 { 6 } else { 10 };
        if position + header > limit { break; }
        let id_len = if version == 2 { 3 } else { 4 };
        if bytes[position..position+id_len].iter().all(|byte| *byte == 0) { break; }
        let id = &bytes[position..position+id_len];
        let frame_size = if version == 2 { (usize::from(bytes[position+3])<<16)|(usize::from(bytes[position+4])<<8)|usize::from(bytes[position+5]) } else if version == 4 { bytes[position+4..position+8].iter().fold(0usize, |acc, byte| (acc << 7) | usize::from(byte & 0x7f)) } else { u32::from_be_bytes(bytes[position+4..position+8].try_into().unwrap()) as usize };
        let end = position.saturating_add(header).saturating_add(frame_size);
        if frame_size == 0 || end > limit { break; }
        if (version == 2 && id == b"PIC") || (version >= 3 && id == b"APIC") { pictures.push((position, end)); }
        position = end;
    }
    let (start, end) = *pictures.get(picture_index).ok_or_else(|| "Coverbild existiert nicht mehr".to_string())?;
    let removed = end - start;
    bytes.drain(start..end);
    let new_size = tag_size.checked_sub(removed).ok_or_else(|| "ID3v2-Größe konnte nicht angepasst werden".to_string())?;
    if new_size >= 0x10000000 { return Err("ID3v2-Tag ist zu groß".into()); }
    bytes[6] = ((new_size >> 21) & 0x7f) as u8; bytes[7] = ((new_size >> 14) & 0x7f) as u8; bytes[8] = ((new_size >> 7) & 0x7f) as u8; bytes[9] = (new_size & 0x7f) as u8;
    Ok(())
}

fn remove_id3_unsynchronization(data: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(data.len());
    let mut index = 0;
    while index < data.len() {
        decoded.push(data[index]);
        if data[index] == 0xFF && data.get(index + 1) == Some(&0x00) {
            let following = data.get(index + 2).copied();
            if following.is_some_and(|byte| byte == 0x00 || byte >= 0xE0) { index += 1; }
        }
        index += 1;
    }
    decoded
}

fn normalize_embedded_image(data: &[u8]) -> Option<Vec<u8>> {
    let signatures: &[&[u8]] = &[b"\xFF\xD8\xFF", b"\x89PNG\r\n\x1A\n", b"GIF87a", b"GIF89a", b"BM", b"II*\0", b"MM\0*", b"RIFF"];
    let offset = signatures.iter().filter_map(|signature| data.windows(signature.len()).position(|window| window == *signature)).min()?;
    let mut image = data[offset..].to_vec();
    if image.starts_with(b"RIFF") && (image.len() < 12 || &image[8..12] != b"WEBP") { return None; }
    if image.starts_with(b"\xFF\xD8\xFF") {
        if let Some(end) = image.windows(2).rposition(|window| window == b"\xFF\xD9") { image.truncate(end + 2); }
    }
    Some(image)
}

fn repair_id3_utf16_frames(bytes: &mut [u8]) -> bool {
    if bytes.len() < 10 || &bytes[..3] != b"ID3" { return false; }
    let version = bytes[3];
    if !(2..=4).contains(&version) { return false; }
    let tag_size = bytes[6..10].iter().fold(0usize, |acc, byte| (acc << 7) | usize::from(byte & 0x7f));
    let mut position = 10usize;
    if version >= 3 && bytes[5] & 0x40 != 0 {
        if position + 4 > bytes.len() { return false; }
        let ext_size = if version == 3 { u32::from_be_bytes(bytes[position..position+4].try_into().unwrap()) as usize } else { bytes[position..position+4].iter().fold(0usize, |acc, byte| (acc << 7) | usize::from(byte & 0x7f)) };
        position = position.saturating_add(if version == 3 { 4 + ext_size } else { ext_size });
    }
    let limit = 10usize.saturating_add(tag_size).min(bytes.len());
    let mut changed = false;
    while position < limit {
        let (header_size, frame_id, frame_size) = if version == 2 {
            if position + 6 > limit { break; }
            let size = (usize::from(bytes[position+3]) << 16) | (usize::from(bytes[position+4]) << 8) | usize::from(bytes[position+5]);
            (6usize, String::from_utf8_lossy(&bytes[position..position+3]).into_owned(), size)
        } else {
            if position + 10 > limit { break; }
            let size = if version == 4 { bytes[position+4..position+8].iter().fold(0usize, |acc, byte| (acc << 7) | usize::from(byte & 0x7f)) } else { u32::from_be_bytes(bytes[position+4..position+8].try_into().unwrap()) as usize };
            (10usize, String::from_utf8_lossy(&bytes[position..position+4]).into_owned(), size)
        };
        if frame_id.as_bytes().iter().all(|byte| *byte == 0) { break; }
        let payload = position.saturating_add(header_size);
        let frame_end = payload.saturating_add(frame_size);
        if frame_size == 0 || frame_end > limit { break; }
        let text_start = if payload >= frame_end { None }
        else if frame_id.starts_with('T') { Some(payload + 1) }
        else if matches!(frame_id.as_str(), "COMM" | "USLT" | "COM" | "ULT" | "USER") { Some(payload + 4) }
        else if matches!(frame_id.as_str(), "APIC" | "PIC") {
            bytes.get(payload+1..frame_end).and_then(|slice| slice.iter().position(|byte| *byte == 0)).map(|mime_end| payload + 1 + mime_end + 2)
        } else { None };
        if let Some(text_start) = text_start {
            if text_start + 2 <= frame_end && bytes[payload] == 1 && !matches!(&bytes[text_start..text_start+2], [0xfe,0xff] | [0xff,0xfe]) {
                let sample_end = (text_start + 34).min(frame_end);
                let pairs = bytes[text_start..sample_end].chunks_exact(2);
                let mut even_zeroes = 0usize; let mut odd_zeroes = 0usize; let mut pair_count = 0usize;
                for pair in pairs { even_zeroes += usize::from(pair[0] == 0); odd_zeroes += usize::from(pair[1] == 0); pair_count += 1; }
                if pair_count > 0 && odd_zeroes > even_zeroes { for pair in bytes[text_start..frame_end].chunks_exact_mut(2) { pair.swap(0,1); } }
                bytes[payload] = 2;
                changed = true;
            }
        }
        position = frame_end;
    }
    changed
}
fn database(app: &tauri::AppHandle) -> Result<Connection, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let db = Connection::open(dir.join("library.sqlite3")).map_err(|e| e.to_string())?;
    db.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS tracks (
           path TEXT PRIMARY KEY, title TEXT NOT NULL, artist TEXT NOT NULL DEFAULT '',
           album TEXT NOT NULL DEFAULT '', album_artist TEXT NOT NULL DEFAULT '', year INTEGER,
           genre TEXT NOT NULL DEFAULT '', track_no INTEGER, disc_no INTEGER, duration_seconds INTEGER NOT NULL DEFAULT 0,
           bitrate INTEGER, sample_rate INTEGER, channels INTEGER, bit_depth INTEGER, format TEXT NOT NULL DEFAULT '',
           extra_tags_json TEXT NOT NULL DEFAULT '{}', track_total INTEGER, disc_total INTEGER, compilation INTEGER NOT NULL DEFAULT 0
         );
         CREATE INDEX IF NOT EXISTS idx_tracks_library_order ON tracks(artist,album,track_no,title);
         CREATE TABLE IF NOT EXISTS playlists (
           id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE TABLE IF NOT EXISTS playlist_tracks (
           playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
           path TEXT NOT NULL REFERENCES tracks(path) ON DELETE CASCADE,
           position INTEGER NOT NULL, PRIMARY KEY (playlist_id, path)
         );
         CREATE TABLE IF NOT EXISTS favorites (
           path TEXT PRIMARY KEY REFERENCES tracks(path) ON UPDATE CASCADE ON DELETE CASCADE,
           added_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE TABLE IF NOT EXISTS library_folders (
           path TEXT PRIMARY KEY, added_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE TABLE IF NOT EXISTS track_analysis (
           path TEXT PRIMARY KEY REFERENCES tracks(path) ON UPDATE CASCADE ON DELETE CASCADE,
           bpm REAL NOT NULL, bpm_confidence REAL NOT NULL, key TEXT NOT NULL,
           key_name TEXT NOT NULL, camelot TEXT NOT NULL, key_confidence REAL NOT NULL,
           beat_offset_seconds REAL NOT NULL, analyzed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE TABLE IF NOT EXISTS tag_history (
           id INTEGER PRIMARY KEY AUTOINCREMENT, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
           summary TEXT NOT NULL, applied INTEGER NOT NULL DEFAULT 1, kind TEXT NOT NULL DEFAULT 'tags'
         );
         CREATE TABLE IF NOT EXISTS tag_history_items (
           history_id INTEGER NOT NULL REFERENCES tag_history(id) ON DELETE CASCADE,
           path TEXT NOT NULL, before_json TEXT NOT NULL, after_json TEXT NOT NULL,
           PRIMARY KEY(history_id,path)
         );
         CREATE TABLE IF NOT EXISTS tag_history_moves (
           history_id INTEGER NOT NULL REFERENCES tag_history(id) ON DELETE CASCADE,
           old_path TEXT NOT NULL, new_path TEXT NOT NULL, PRIMARY KEY(history_id,old_path)
         );
         CREATE TABLE IF NOT EXISTS track_artwork_cache (
           path TEXT PRIMARY KEY, mime TEXT NOT NULL, image BLOB NOT NULL, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );"
    ).map_err(|e| e.to_string())?;
    let has_kind: bool = {
        let mut stmt = db
            .prepare("PRAGMA table_info(tag_history)")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(|e| e.to_string())?;
        let present = rows.filter_map(Result::ok).any(|n| n == "kind");
        present
    };
    if !has_kind {
        db.execute(
            "ALTER TABLE tag_history ADD COLUMN kind TEXT NOT NULL DEFAULT 'tags'",
            [],
        )
        .map_err(|e| e.to_string())?;
    }
    let columns: Vec<String> = {
        let mut stmt = db
            .prepare("PRAGMA table_info(tracks)")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(|e| e.to_string())?;
        rows.filter_map(Result::ok).collect()
    };
    for (name, definition) in [
        ("extra_tags_json", "TEXT NOT NULL DEFAULT '{}'"),
        ("track_total", "INTEGER"),
        ("disc_total", "INTEGER"),
        ("compilation", "INTEGER NOT NULL DEFAULT 0"),
        ("channels", "INTEGER"),
        ("bit_depth", "INTEGER"),
    ] {
        if !columns.iter().any(|c| c == name) {
            db.execute(
                &format!("ALTER TABLE tracks ADD COLUMN {name} {definition}"),
                [],
            )
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(db)
}

fn write_options_for(path: &Path) -> WriteOptions {
    let mut header = [0u8; 4];
    let is_v23 = std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .is_ok()
        && &header[..3] == b"ID3"
        && header[3] == 3;
    let mut options = WriteOptions::default();
    options.use_id3v23(is_v23)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub path: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub year: Option<u32>,
    pub genre: String,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    #[serde(default)]
    pub track_total: Option<u32>,
    #[serde(default)]
    pub disc_total: Option<u32>,
    #[serde(default)]
    pub compilation: bool,
    #[serde(default)]
    pub extra_tags: BTreeMap<String, String>,
    pub duration_seconds: u64,
    pub bitrate: Option<u32>,
    pub sample_rate: Option<u32>,
    #[serde(default)]
    pub channels: Option<u8>,
    #[serde(default)]
    pub bit_depth: Option<u8>,
    pub format: String,
}

fn supported(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "mp3"
            | "flac"
            | "ogg"
            | "opus"
            | "m4a"
            | "mp4"
            | "aif"
            | "aiff"
            | "wav"
            | "ape"
            | "wma"
            | "wv"
            | "mpc"
            | "dsf"
            | "tta"
    )
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DuplicateGroup {
    size_bytes: u64,
    paths: Vec<String>,
}

fn hash_file(path: &Path) -> Result<(u64, u64), String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    let mut hasher = DefaultHasher::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if count == 0 {
            break;
        }
        buffer[..count].hash(&mut hasher);
    }
    Ok((size, hasher.finish()))
}

fn files_identical(left: &Path, right: &Path) -> Result<bool, String> {
    let mut a = std::fs::File::open(left).map_err(|e| e.to_string())?;
    let mut b = std::fs::File::open(right).map_err(|e| e.to_string())?;
    let mut x = [0u8; 65536];
    let mut y = [0u8; 65536];
    loop {
        let nx = a.read(&mut x).map_err(|e| e.to_string())?;
        let ny = b.read(&mut y).map_err(|e| e.to_string())?;
        if nx != ny || x[..nx] != y[..ny] {
            return Ok(false);
        }
        if nx == 0 {
            return Ok(true);
        }
    }
}

#[tauri::command]
async fn find_duplicates(paths: Vec<String>) -> Result<Vec<DuplicateGroup>, String> {
    tauri::async_runtime::spawn_blocking(move || find_duplicates_sync(paths))
        .await
        .map_err(|e| format!("Duplikat-Scan konnte nicht ausgeführt werden: {e}"))?
}

fn find_duplicates_sync(paths: Vec<String>) -> Result<Vec<DuplicateGroup>, String> {
    let mut candidates: BTreeMap<(u64, u64), Vec<String>> = BTreeMap::new();
    for raw in paths {
        let path = PathBuf::from(raw);
        if !path.is_file() {
            continue;
        }
        let key = hash_file(&path)?;
        candidates
            .entry(key)
            .or_default()
            .push(path.to_string_lossy().into_owned());
    }
    let mut result = Vec::new();
    for ((size, _), items) in candidates {
        if items.len() < 2 {
            continue;
        }
        let mut groups: Vec<Vec<String>> = Vec::new();
        for path in items {
            let mut matched = false;
            for group in &mut groups {
                if files_identical(Path::new(&group[0]), Path::new(&path))? {
                    group.push(path.clone());
                    matched = true;
                    break;
                }
            }
            if !matched {
                groups.push(vec![path]);
            }
        }
        for paths in groups.into_iter().filter(|g| g.len() > 1) {
            result.push(DuplicateGroup {
                size_bytes: size,
                paths,
            });
        }
    }
    Ok(result)
}

#[tauri::command]
async fn trash_duplicate_copies(
    app: tauri::AppHandle,
    keep_path: String,
    duplicate_paths: Vec<String>,
) -> Result<Vec<(String, Result<(), String>)>, String> {
    tauri::async_runtime::spawn_blocking(move||{
        let keep=PathBuf::from(&keep_path);
        if !keep.is_file(){return Err(format!("Behaltene Datei existiert nicht mehr: {}",keep.display()))}
        let db=database(&app)?;let mut outcomes=Vec::new();
        for raw in duplicate_paths{
            let path=PathBuf::from(&raw);
            let result=(||{
                if path==keep{return Err("Die ausgewählte Originaldatei kann nicht als Duplikat entfernt werden".into())}
                let metadata=std::fs::symlink_metadata(&path).map_err(|e|e.to_string())?;
                if metadata.file_type().is_symlink()||!metadata.is_file(){return Err("Nur reguläre Audiodateien können in den Papierkorb verschoben werden".into())}
                if !files_identical(&keep,&path)?{return Err("Die Datei stimmt bytegenau nicht mehr mit dem behaltenen Titel überein; bitte erneut scannen".into())}
                trash::delete(&path).map_err(|e|format!("Papierkorb konnte die Datei nicht aufnehmen: {e}"))?;
                db.execute("DELETE FROM tracks WHERE path=?1",params![path.to_string_lossy()]).map_err(|e|format!("Datei liegt im Papierkorb, Bibliothek konnte aber nicht aktualisiert werden: {e}"))?;
                Ok(())
            })();
            outcomes.push((raw,result));
        }
        Ok(outcomes)
    }).await.map_err(|e|format!("Papierkorb-Aktion konnte nicht ausgeführt werden: {e}"))?
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RenamePreview {
    old_path: String,
    new_path: String,
    title: String,
    artist: String,
}

fn filename_component(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.').trim_end_matches(' ');
    if trimmed.is_empty() {
        "Unbekannt".to_string()
    } else {
        trimmed.to_string()
    }
}

fn rename_preview(paths: Vec<String>, pattern: &str) -> Result<Vec<RenamePreview>, String> {
    let mut out = Vec::new();
    for raw in paths {
        let path = PathBuf::from(raw);
        let track = read_one(&path)?;
        let year = track.year.map(|v| v.to_string()).unwrap_or_default();
        let track_no = track.track.map(|v| format!("{v:02}")).unwrap_or_default();
        let mut relative = pattern.to_string();
        for (token, value) in [
            ("{artist}", track.artist.as_str()),
            ("{album}", track.album.as_str()),
            ("{title}", track.title.as_str()),
            ("{year}", year.as_str()),
            ("{track}", track_no.as_str()),
        ] {
            relative = relative.replace(token, &filename_component(value));
        }
        if relative.starts_with('/') || relative.starts_with('\\') || relative.contains(':') {
            return Err("Das Namensmuster muss relativ zum aktuellen Ordner sein".into());
        }
        let mut segments = Vec::new();
        for segment in relative.split(['/', '\\']) {
            if segment.is_empty() || segment == "." || segment == ".." {
                return Err("Das Namensmuster enthält einen ungültigen Ordnerabschnitt".into());
            }
            segments.push(filename_component(segment));
        }
        let name = segments
            .pop()
            .ok_or_else(|| "Das Namensmuster ist leer".to_string())?;
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let file_name = if ext.is_empty() {
            name
        } else {
            format!("{name}.{ext}")
        };
        segments.push(file_name);
        let parent = path
            .parent()
            .ok_or_else(|| "Quelldatei hat keinen gültigen Ordner".to_string())?;
        let new_path = segments
            .iter()
            .fold(parent.to_path_buf(), |p, s| p.join(s))
            .to_string_lossy()
            .to_string();
        out.push(RenamePreview {
            old_path: path.to_string_lossy().to_string(),
            new_path,
            title: track.title,
            artist: track.artist,
        });
    }
    let sources: std::collections::HashSet<String> =
        out.iter().map(|p| p.old_path.to_lowercase()).collect();
    let mut destinations = std::collections::HashSet::new();
    for p in &out {
        if !destinations.insert(p.new_path.to_lowercase()) {
            return Err(format!(
                "Mehrere Dateien würden denselben Namen erhalten: {}",
                p.new_path
            ));
        }
        if p.old_path != p.new_path
            && Path::new(&p.new_path).exists()
            && !sources.contains(&p.new_path.to_lowercase())
        {
            return Err(format!("Zieldatei existiert bereits: {}", p.new_path));
        }
    }
    Ok(out)
}

#[tauri::command]
fn preview_rename(paths: Vec<String>, pattern: String) -> Result<Vec<RenamePreview>, String> {
    rename_preview(paths, &pattern)
}

#[tauri::command]
fn rename_from_tags(
    app: tauri::AppHandle,
    paths: Vec<String>,
    pattern: String,
) -> Result<Vec<RenamePreview>, String> {
    let preview = rename_preview(paths, &pattern)?;
    let mut moves = Vec::new();
    for item in &preview {
        if item.old_path == item.new_path {
            continue;
        }
        let old = PathBuf::from(&item.old_path);
        let new = PathBuf::from(&item.new_path);
        if let Some(parent) = new.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Zielordner konnte nicht erstellt werden: {e}"))?;
        }
        let mut temp = old.clone();
        temp.set_extension(format!("metamagic-rename-{}.tmp", std::process::id()));
        if temp.exists() {
            for (staged_old, staged_temp, _) in moves.iter().rev() {
                let _ = std::fs::rename(staged_temp, staged_old);
            }
            return Err(format!(
                "Temporäre Datei existiert bereits: {}",
                temp.display()
            ));
        }
        if let Err(e) = std::fs::rename(&old, &temp) {
            for (staged_old, staged_temp, _) in moves.iter().rev() {
                let _ = std::fs::rename(staged_temp, staged_old);
            }
            return Err(format!(
                "{} konnte nicht vorbereitet werden: {e}",
                old.display()
            ));
        }
        moves.push((old, temp, new));
    }
    let mut completed: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (old, temp, new) in moves {
        if let Err(e) = std::fs::rename(&temp, &new) {
            let _ = std::fs::rename(&temp, &old);
            for (done_old, done_new) in completed.iter().rev() {
                let _ = std::fs::rename(done_new, done_old);
            }
            return Err(format!(
                "Umbenennen nach {} fehlgeschlagen: {e}",
                new.display()
            ));
        }
        completed.push((old, new));
    }
    let db = database(&app)?;
    let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
    tx.execute("DELETE FROM tag_history WHERE applied=0", [])
        .map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO tag_history(summary,applied,kind) VALUES (?1,1,'rename')",
        params![format!(
            "{} Datei(en) anhand ihrer Tags umbenannt",
            preview.len()
        )],
    )
    .map_err(|e| e.to_string())?;
    let history_id = tx.last_insert_rowid();
    for (old, new) in &completed {
        let playlist_rows: Vec<(i64, i64)> = {
            let mut stmt = tx
                .prepare("SELECT playlist_id,position FROM playlist_tracks WHERE path=?1")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![old.to_string_lossy()], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .map_err(|e| e.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?
        };
        tx.execute(
            "DELETE FROM playlist_tracks WHERE path=?1",
            params![old.to_string_lossy()],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE tracks SET path=?2 WHERE path=?1",
            params![old.to_string_lossy(), new.to_string_lossy()],
        )
        .map_err(|e| e.to_string())?;
        for (playlist_id, position) in playlist_rows {
            tx.execute(
                "INSERT INTO playlist_tracks(playlist_id,path,position) VALUES (?1,?2,?3)",
                params![playlist_id, new.to_string_lossy(), position],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.execute(
            "INSERT INTO tag_history_moves(history_id,old_path,new_path) VALUES (?1,?2,?3)",
            params![history_id, old.to_string_lossy(), new.to_string_lossy()],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(preview)
}

fn read_one(path: &Path) -> Result<Track, String> {
    let tagged = read_tagged(path)?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    let properties = tagged.properties();
    let get = |key| {
        tag.and_then(|t| t.get_string(&key))
            .unwrap_or_default()
            .to_owned()
    };
    let extra_keys = [
        ("lyrics", ItemKey::Lyrics),
        ("composer", ItemKey::Composer),
        ("work", ItemKey::Work),
        ("movement", ItemKey::Movement),
        ("movementNumber", ItemKey::MovementNumber),
        ("movementTotal", ItemKey::MovementTotal),
        ("grouping", ItemKey::ContentGroup),
        ("comments", ItemKey::Comment),
        ("bpm", ItemKey::IntegerBpm),
        ("key", ItemKey::InitialKey),
        ("mood", ItemKey::Mood),
        ("remixer", ItemKey::Remixer),
        ("producer", ItemKey::Producer),
        ("publisher", ItemKey::Publisher),
        ("label", ItemKey::Label),
        ("isrc", ItemKey::Isrc),
        ("barcode", ItemKey::Barcode),
        ("catalogNumber", ItemKey::CatalogNumber),
        ("releaseDate", ItemKey::ReleaseDate),
        ("originalReleaseDate", ItemKey::OriginalReleaseDate),
        ("copyright", ItemKey::CopyrightMessage),
        ("language", ItemKey::Language),
        ("encodedBy", ItemKey::EncodedBy),
    ];
    let extra_tags = extra_keys
        .iter()
        .filter_map(|(name, key)| {
            tag.and_then(|t| t.get_string(key))
                .filter(|v| !v.is_empty())
                .map(|v| ((*name).to_owned(), v.to_owned()))
        })
        .collect();
    let compilation = tag
        .and_then(|t| t.get_string(&ItemKey::FlagCompilation))
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    Ok(Track {
        path: path.to_string_lossy().into_owned(),
        title: tag
            .and_then(|t| t.title())
            .map(|s| s.into_owned())
            .unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }),
        artist: get(ItemKey::TrackArtist),
        album: get(ItemKey::AlbumTitle),
        album_artist: get(ItemKey::AlbumArtist),
        year: tag.and_then(|t| t.year()),
        genre: get(ItemKey::Genre),
        track: tag.and_then(|t| t.track()),
        disc: tag.and_then(|t| t.disk()),
        track_total: tag.and_then(|t| t.track_total()),
        disc_total: tag.and_then(|t| t.disk_total()),
        compilation,
        extra_tags,
        duration_seconds: properties.duration().as_secs(),
        bitrate: properties.audio_bitrate(),
        sample_rate: properties.sample_rate(),
        channels: properties.channels(),
        bit_depth: properties.bit_depth(),
        format: path
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or("")
            .to_ascii_uppercase(),
    })
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let a = chunk[0] as u32;
        let b = *chunk.get(1).unwrap_or(&0) as u32;
        let c = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (a << 16) | (b << 8) | c;
        output.push(TABLE[((n >> 18) & 63) as usize] as char);
        output.push(TABLE[((n >> 12) & 63) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}

fn image_mime(data: &[u8], hint: Option<&str>) -> String {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return "image/jpeg".into();
    }
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return "image/png".into();
    }
    if data.starts_with(b"GIF8") {
        return "image/gif".into();
    }
    if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return "image/webp".into();
    }
    if data.starts_with(b"BM") {
        return "image/bmp".into();
    }
    if data.starts_with(b"II*\0") || data.starts_with(b"MM\0*") {
        return "image/tiff".into();
    }
    hint.filter(|value| value.starts_with("image/") && value.len() > 6)
        .unwrap_or("image/jpeg")
        .to_owned()
}

#[tauri::command]
fn get_track_artwork(app: tauri::AppHandle, path: String) -> Result<Option<String>, String> {
    let audio_path = PathBuf::from(&path);
    if let Ok(bytes) = std::fs::read(&audio_path) {
        let pictures = id3_picture_frames(&bytes);
        if let Some((_, mime, data)) = pictures.iter().find(|(kind, _, _)| *kind == PictureType::CoverFront).or_else(|| pictures.first()) {
            let mime = image_mime(data, Some(mime));
            return Ok(Some(format!("data:{mime};base64,{}", base64_encode(data))));
        }
    }
    if let Ok(db) = database(&app) {
        let cached: Option<(String, Vec<u8>)> = db.query_row("SELECT mime,image FROM track_artwork_cache WHERE path=?1", [&path], |row| Ok((row.get(0)?,row.get(1)?))).optional().map_err(|e| e.to_string())?;
        if let Some((mime, image)) = cached.filter(|(_, image)| !image.is_empty()) {
            return Ok(Some(format!("data:{mime};base64,{}", base64_encode(&image))));
        }
    }
    if let Some(folder) = audio_path.parent() {
        for name in [
            "cover.jpg",
            "cover.jpeg",
            "cover.png",
            "cover.webp",
            "folder.jpg",
            "folder.jpeg",
            "folder.png",
            "folder.webp",
            "front.jpg",
            "front.jpeg",
            "front.png",
            "album.jpg",
            "album.png",
        ] {
            let candidate = folder.join(name);
            if let Ok(bytes) = std::fs::read(&candidate) {
                if !bytes.is_empty() {
                    let hint = match candidate
                        .extension()
                        .and_then(|x| x.to_str())
                        .unwrap_or("")
                        .to_ascii_lowercase()
                        .as_str()
                    {
                        "png" => Some("image/png"),
                        "webp" => Some("image/webp"),
                        "gif" => Some("image/gif"),
                        "bmp" => Some("image/bmp"),
                        "tif" | "tiff" => Some("image/tiff"),
                        _ => None,
                    };
                    let mime = image_mime(&bytes, hint);
                    return Ok(Some(format!(
                        "data:{mime};base64,{}",
                        base64_encode(&bytes)
                    )));
                }
            }
        }
    }
    let tagged = match read_tagged(&audio_path) {
        Ok(tagged) => tagged,
        Err(error) if FileType::from_path(&audio_path) == Some(FileType::Mpeg) && error.contains("UTF-16 string has an invalid byte order mark") => {
            let mut bytes = std::fs::read(&audio_path).map_err(|e| e.to_string())?;
            if strip_leading_id3v2(&mut bytes) {
                Probe::with_file_type(Cursor::new(bytes), FileType::Mpeg).options(ParseOptions::new().read_tags(false).parsing_mode(ParsingMode::Relaxed)).read().map_err(|e| e.to_string())?
            } else { return Err(error); }
        },
        Err(error) => return Err(error),
    };
    let picture = tagged
        .tags()
        .iter()
        .find_map(|tag| tag.get_picture_type(PictureType::CoverFront))
        .or_else(|| tagged.tags().iter().find_map(|tag| tag.pictures().first()));
    let Some(picture) = picture else {
        return Ok(None);
    };
    let data = picture.data();
    if data.is_empty() {
        return Ok(None);
    }
    let mime = image_mime(data, picture.mime_type().map(|m| m.as_str()));
    Ok(Some(format!("data:{mime};base64,{}", base64_encode(data))))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TrackPicture {
    data_url: String,
    picture_type: String,
    description: String,
}

fn picture_type_name(kind: PictureType) -> String {
    match kind {
        PictureType::Other => "Other",
        PictureType::Icon => "Icon",
        PictureType::OtherIcon => "OtherIcon",
        PictureType::CoverFront => "CoverFront",
        PictureType::CoverBack => "CoverBack",
        PictureType::Leaflet => "Leaflet",
        PictureType::Media => "Media",
        PictureType::LeadArtist => "LeadArtist",
        PictureType::Artist => "Artist",
        PictureType::Conductor => "Conductor",
        PictureType::Band => "Band",
        PictureType::Composer => "Composer",
        PictureType::Lyricist => "Lyricist",
        PictureType::RecordingLocation => "RecordingLocation",
        PictureType::DuringRecording => "DuringRecording",
        PictureType::DuringPerformance => "DuringPerformance",
        PictureType::ScreenCapture => "ScreenCapture",
        PictureType::BrightFish => "BrightFish",
        PictureType::Illustration => "Illustration",
        PictureType::BandLogo => "BandLogo",
        PictureType::PublisherLogo => "PublisherLogo",
        PictureType::Undefined(_) => "Other",
        _ => "Other",
    }
    .to_owned()
}

fn parse_picture_type(name: &str) -> PictureType {
    match name {
        "CoverFront" => PictureType::CoverFront,
        "CoverBack" => PictureType::CoverBack,
        "Leaflet" => PictureType::Leaflet,
        "Media" => PictureType::Media,
        "LeadArtist" => PictureType::LeadArtist,
        "Artist" => PictureType::Artist,
        "Conductor" => PictureType::Conductor,
        "Band" => PictureType::Band,
        "Composer" => PictureType::Composer,
        "Lyricist" => PictureType::Lyricist,
        "RecordingLocation" => PictureType::RecordingLocation,
        "DuringRecording" => PictureType::DuringRecording,
        "DuringPerformance" => PictureType::DuringPerformance,
        "ScreenCapture" => PictureType::ScreenCapture,
        "Illustration" => PictureType::Illustration,
        "BandLogo" => PictureType::BandLogo,
        "PublisherLogo" => PictureType::PublisherLogo,
        "Icon" => PictureType::Icon,
        "OtherIcon" => PictureType::OtherIcon,
        _ => PictureType::Other,
    }
}

#[tauri::command]
fn get_track_pictures(path: String) -> Result<Vec<TrackPicture>, String> {
    if let Ok(bytes) = std::fs::read(&path) {
        let raw_pictures = id3_picture_frames(&bytes);
        if !raw_pictures.is_empty() {
            return Ok(raw_pictures.into_iter().map(|(kind, mime, data)| TrackPicture { data_url: format!("data:{};base64,{}", image_mime(&data, Some(&mime)), base64_encode(&data)), picture_type: picture_type_name(kind), description: String::new() }).collect());
        }
    }
    let tagged = read_tagged(Path::new(&path))?;
    Ok(tagged
        .tags()
        .iter()
        .flat_map(|tag| tag.pictures())
        .filter(|picture| !picture.data().is_empty())
        .map(|picture| {
            let mime = image_mime(picture.data(), picture.mime_type().map(|m| m.as_str()));
            TrackPicture {
                data_url: format!("data:{mime};base64,{}", base64_encode(picture.data())),
                picture_type: picture_type_name(picture.pic_type()),
                description: picture.description().unwrap_or_default().to_owned(),
            }
        })
        .collect())
}

#[tauri::command]
fn add_track_picture(path: String, image_path: String, picture_type: String) -> Result<(), String> {
    let path = PathBuf::from(path);
    let image = std::fs::read(image_path).map_err(|e| e.to_string())?;
    let mime = image_mime(&image, None);
    if !mime.starts_with("image/") || image.len() < 8 {
        return Err("Die ausgewählte Datei ist kein unterstütztes Bild".into());
    }
    let mut tagged = read_tagged(&path)?;
    let tag_type = tagged.primary_tag_type();
    if tagged.tag(tag_type).is_none() {
        tagged.insert_tag(Tag::new(tag_type));
    }
    let tag = tagged
        .tag_mut(tag_type)
        .ok_or_else(|| "Dateiformat unterstützt keine eingebetteten Bilder".to_string())?;
    tag.push_picture(Picture::new_unchecked(
        parse_picture_type(&picture_type),
        Some(MimeType::from_str(&mime)),
        None,
        image,
    ));
    tagged
        .save_to_path(&path, write_options_for(&path))
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn remove_track_picture(app: tauri::AppHandle, path: String, index: usize) -> Result<(), String> {
    let path = PathBuf::from(path);
    if FileType::from_path(&path) == Some(FileType::Mpeg) {
        let mut bytes = std::fs::read(&path).map_err(|e| format!("MP3 konnte nicht gelesen werden: {e}"))?;
        if !id3_picture_frames(&bytes).is_empty() {
            remove_id3_picture_frame(&mut bytes, index)?;
                    std::fs::write(&path, bytes).map_err(|e| format!("Cover konnte nicht aus dem ID3-Tag entfernt werden: {e}"))?;
            database(&app)?.execute("DELETE FROM track_artwork_cache WHERE path=?1", [path.to_string_lossy().as_ref()]).map_err(|e| e.to_string())?;
            return Ok(());
        }
    }
    let mut tagged = read_tagged(&path)?;
    let tag_type = tagged.tags().iter().find(|tag| index < tag.pictures().len()).map(|tag| tag.tag_type()).ok_or_else(|| "Coverbild existiert nicht mehr".to_string())?;
    let tag = tagged.tag_mut(tag_type).ok_or_else(|| "Tag nicht gefunden".to_string())?;
    tag.remove_picture(index);
    tagged.save_to_path(&path, write_options_for(&path)).map_err(|e| e.to_string())?;
    database(&app)?.execute("DELETE FROM track_artwork_cache WHERE path=?1", [path.to_string_lossy().as_ref()]).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn set_track_picture_type(path: String, index: usize, picture_type: String) -> Result<(), String> {
    let path = PathBuf::from(path);
    let mut tagged = read_tagged(&path)?;
    let tag_type = tagged.primary_tag_type();
    let tag = tagged
        .tag_mut(tag_type)
        .ok_or_else(|| "Tag nicht gefunden".to_string())?;
    let mut picture = tag
        .pictures()
        .get(index)
        .ok_or_else(|| "Coverbild existiert nicht mehr".to_string())?
        .clone();
    picture.set_pic_type(parse_picture_type(&picture_type));
    tag.set_picture(index, picture);
    tagged
        .save_to_path(&path, write_options_for(&path))
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn export_folder_artwork(path: String, overwrite: bool) -> Result<String, String> {
    let audio_path = PathBuf::from(&path);
    let tagged = read_tagged(&audio_path)?;
    let picture = tagged
        .tags()
        .iter()
        .find_map(|tag| tag.get_picture_type(PictureType::CoverFront))
        .or_else(|| tagged.tags().iter().find_map(|tag| tag.pictures().first()))
        .ok_or_else(|| "In dieser Audiodatei ist kein Coverbild eingebettet".to_string())?;
    let ext = picture
        .mime_type()
        .and_then(|mime| mime.ext())
        .unwrap_or_else(|| {
            let data = picture.data();
            if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
                "jpg"
            } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
                "png"
            } else {
                "jpg"
            }
        });
    let folder = audio_path
        .parent()
        .ok_or_else(|| "Der Audiodatei fehlt ein gültiger Ordner".to_string())?;
    let destination = folder.join(format!("folder.{ext}"));
    if destination.exists() && !overwrite {
        return Err(format!("EXISTS:{}", destination.to_string_lossy()));
    }
    let mut temp = destination.clone();
    temp.set_extension(format!("{ext}.metamagic-{}.tmp", std::process::id()));
    std::fs::write(&temp, picture.data())
        .map_err(|e| format!("Ordnerbild konnte nicht geschrieben werden: {e}"))?;
    let mut previous = destination.clone();
    previous.set_extension(format!("{ext}.metamagic-{}.bak", std::process::id()));
    if destination.exists() {
        if let Err(error) = std::fs::rename(&destination, &previous) {
            let _ = std::fs::remove_file(&temp);
            return Err(format!(
                "Vorhandenes Ordnerbild konnte nicht gesichert werden: {error}"
            ));
        }
    }
    if let Err(error) = std::fs::rename(&temp, &destination) {
        let _ = std::fs::remove_file(&temp);
        if previous.exists() {
            let _ = std::fs::rename(&previous, &destination);
        }
        return Err(format!(
            "Ordnerbild konnte nicht gespeichert werden: {error}"
        ));
    }
    if previous.exists() {
        let _ = std::fs::remove_file(previous);
    }
    Ok(destination.to_string_lossy().into_owned())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanProgress {
    phase: String,
    current: usize,
    total: Option<usize>,
    succeeded: usize,
    failed: usize,
    current_file: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanSummary {
    discovered: usize,
    imported: usize,
    failed: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MusicBrainzCandidate {
    id: String,
    score: u8,
    title: String,
    artist: String,
    album: String,
    album_artist: String,
    year: String,
    track: String,
    track_total: String,
    disc: String,
    disc_total: String,
    compilation: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CoverSearchResult {
    id: String,
    source: String,
    title: String,
    artist: String,
    year: String,
    image_url: String,
    thumbnail_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LyricsCandidate {
    id: u64,
    track_name: String,
    artist_name: String,
    album_name: String,
    duration: u64,
    instrumental: bool,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
}

static MUSICBRAINZ_NEXT_REQUEST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
static LRCLIB_NEXT_REQUEST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
static LRCLIB_REQUEST_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn wait_for_musicbrainz_slot() -> Result<(), String> {
    let next = MUSICBRAINZ_NEXT_REQUEST.get_or_init(|| Mutex::new(None));
    let wait = {
        let mut scheduled = next.lock().map_err(|e| e.to_string())?;
        let now = Instant::now();
        let wait = scheduled
            .map(|time| time.saturating_duration_since(now))
            .unwrap_or_default();
        *scheduled = Some(now + wait + Duration::from_secs(1));
        wait
    };
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
    Ok(())
}

async fn wait_for_lrclib_slot() -> Result<(), String> {
    let next = LRCLIB_NEXT_REQUEST.get_or_init(|| Mutex::new(None));
    let wait = {
        let scheduled = next.lock().map_err(|e| e.to_string())?;
        let now = Instant::now();
        scheduled
            .map(|time| time.saturating_duration_since(now))
            .unwrap_or_default()
    };
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
    Ok(())
}

fn apply_lrclib_backoff(seconds: u64) -> Result<(), String> {
    let jitter_seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let base_ms = seconds.saturating_mul(1000);
    let jitter_ms = base_ms.saturating_mul(jitter_seed % 301) / 1000;
    let retry_at = Instant::now() + Duration::from_millis(base_ms.saturating_add(jitter_ms));
    let mut scheduled = LRCLIB_NEXT_REQUEST
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|e| e.to_string())?;
    if scheduled.is_none_or(|current| current < retry_at) {
        *scheduled = Some(retry_at);
    }
    Ok(())
}

fn lrclib_retry_after(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

struct LrclibResponse {
    status: reqwest::StatusCode,
    retry_after: Option<u64>,
    body: Vec<u8>,
}

async fn lrclib_request(endpoint: &str, query: &[(&str, &str)]) -> Result<LrclibResponse, String> {
    let _request_lock = LRCLIB_REQUEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    wait_for_lrclib_slot().await?;
    let response = reqwest::Client::builder()
        .user_agent("MetaMagic/0.1.0 (desktop music manager)")
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("Lyrics-Suche konnte nicht gestartet werden: {e}"))?
        .get(format!("https://lrclib.net{endpoint}"))
        .query(query)
        .send()
        .await
        .map_err(|e| format!("LRCLIB ist nicht erreichbar: {e}"))?;
    let status = response.status();
    let retry_after = lrclib_retry_after(&response);
    if let Some(seconds) = retry_after {
        apply_lrclib_backoff(seconds)?;
    }
    let body = response
        .bytes()
        .await
        .map_err(|e| format!("LRCLIB-Antwort konnte nicht gelesen werden: {e}"))?
        .to_vec();
    let next_allowed = Instant::now() + Duration::from_millis(750);
    let mut scheduled = LRCLIB_NEXT_REQUEST
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|e| e.to_string())?;
    if scheduled.is_none_or(|current| current < next_allowed) {
        *scheduled = Some(next_allowed);
    }
    Ok(LrclibResponse {
        status,
        retry_after,
        body,
    })
}

#[tauri::command]
async fn search_lyrics(
    title: String,
    artist: String,
    album: String,
    duration: u64,
) -> Result<Vec<LyricsCandidate>, String> {
    let title = title.trim();
    let artist = artist.trim();
    let album = album.trim();
    if title.is_empty() {
        return Err("Ein Titel wird für die Lyrics-Suche benötigt".into());
    }

    let duration_text = duration.to_string();
    if !artist.is_empty() {
        let mut exact_query = vec![("track_name", title), ("artist_name", artist)];
        if !album.is_empty() {
            exact_query.push(("album_name", album));
        }
        if (1..=3600).contains(&duration) {
            exact_query.push(("duration", duration_text.as_str()));
        }
        let response = lrclib_request("/api/get", &exact_query).await?;
        if response.status.is_success() {
            let candidate = serde_json::from_slice::<LyricsCandidate>(&response.body)
                .map_err(|e| format!("Lyrics-Antwort konnte nicht gelesen werden: {e}"))?;
            return Ok(vec![candidate]);
        }
        if response.status != reqwest::StatusCode::NOT_FOUND {
            let retry_after = response
                .retry_after
                .map(|value| format!(" (Retry-After: {value} s)"))
                .unwrap_or_default();
            return Err(format!(
                "LRCLIB meldete HTTP {}{retry_after}",
                response.status
            ));
        }
    }

    let mut search_query = vec![("track_name", title)];
    if !artist.is_empty() {
        search_query.push(("artist_name", artist));
    }
    if !album.is_empty() {
        search_query.push(("album_name", album));
    }
    let response = lrclib_request("/api/search", &search_query).await?;
    if !response.status.is_success() {
        let retry_after = response
            .retry_after
            .map(|value| format!(" · erneuter Versuch frühestens in {value} s"))
            .unwrap_or_default();
        return Err(format!(
            "Lyrics-Suche fehlgeschlagen (HTTP {}){retry_after}",
            response.status
        ));
    }
    serde_json::from_slice::<Vec<LyricsCandidate>>(&response.body)
        .map_err(|e| format!("Lyrics-Suchergebnisse konnten nicht gelesen werden: {e}"))
}

#[tauri::command]
fn save_lyrics_sidecar(
    path: String,
    lyrics: String,
    synced: bool,
    overwrite: bool,
) -> Result<String, String> {
    if lyrics.trim().is_empty() {
        return Err("Der Liedtext ist leer".into());
    }
    let source = PathBuf::from(path);
    if !source.is_file() {
        return Err("Die ausgewählte Audiodatei wurde nicht gefunden".into());
    }
    let mut destination = source;
    destination.set_extension(if synced { "lrc" } else { "txt" });
    if destination.exists() && !overwrite {
        return Err(format!("EXISTS:{}", destination.to_string_lossy()));
    }
    std::fs::write(&destination, lyrics.as_bytes())
        .map_err(|e| format!("Lyrics-Datei konnte nicht gespeichert werden: {e}"))?;
    Ok(destination.to_string_lossy().into_owned())
}

fn musicbrainz_credit(value: &serde_json::Value) -> String {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    format!(
                        "{}{}",
                        item["name"]
                            .as_str()
                            .or_else(|| item["artist"]["name"].as_str())
                            .unwrap_or_default(),
                        item["joinphrase"].as_str().unwrap_or_default()
                    )
                })
                .collect::<String>()
        })
        .unwrap_or_default()
}

#[tauri::command]
async fn search_musicbrainz(
    title: String,
    artist: String,
    album: String,
) -> Result<Vec<MusicBrainzCandidate>, String> {
    let title = title.trim();
    let artist = artist.trim();
    let album = album.trim();
    if title.is_empty() && artist.is_empty() {
        return Err("Gib mindestens Titel oder Interpret für die Suche ein".into());
    }
    let escape = |value: &str| value.replace('\\', "\\\\").replace('"', "\\\"");
    let mut terms = Vec::new();
    if !title.is_empty() {
        terms.push(format!("recording:\"{}\"", escape(title)));
    }
    if !artist.is_empty() {
        terms.push(format!("artist:\"{}\"", escape(artist)));
    }
    if !album.is_empty() {
        terms.push(format!("release:\"{}\"", escape(album)));
    }
    let query = terms.join(" AND ");
    let client = reqwest::Client::builder()
        .user_agent("MetaMagic/0.1.0 (desktop music metadata lookup)")
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("Online-Suche konnte nicht gestartet werden: {e}"))?;
    let mut response = None;
    for attempt in 0..3 {
        wait_for_musicbrainz_slot().await?;
        let result = client
            .get("https://musicbrainz.org/ws/2/recording/")
            .query(&[
                ("query", query.as_str()),
                ("fmt", "json"),
                ("limit", "10"),
                ("inc", "releases"),
            ])
            .send()
            .await
            .map_err(|e| format!("MusicBrainz ist nicht erreichbar: {e}"))?;
        if result.status().is_success() {
            response = Some(result.json::<serde_json::Value>().await.map_err(|e| format!("MusicBrainz-Antwort konnte nicht gelesen werden: {e}"))?);
            break;
        }
        let status = result.status();
        if (status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS) && attempt < 2 {
            let retry_after = result.headers().get(reqwest::header::RETRY_AFTER).and_then(|value| value.to_str().ok()).and_then(|value| value.parse::<u64>().ok()).unwrap_or(1).clamp(1, 8);
            tokio::time::sleep(Duration::from_secs(retry_after)).await;
            continue;
        }
        if status.is_server_error() {
            return Err(format!("MusicBrainz ist momentan nicht verfügbar (HTTP {status}). Bitte versuche es in einigen Minuten erneut."));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err("MusicBrainz begrenzt gerade die Suchanfragen. Bitte warte kurz und versuche es erneut.".into());
        }
        return Err(format!("MusicBrainz-Suche wurde abgelehnt (HTTP {status}). Bitte prüfe die Suchbegriffe."));
    }
    let response = response.ok_or_else(|| "MusicBrainz blieb nach mehreren Versuchen nicht erreichbar. Bitte versuche es später erneut.".to_string())?;

    let mut candidates = Vec::new();
    if let Some(recordings) = response["recordings"].as_array() {
        for recording in recordings {
            let releases = recording["releases"].as_array();
            let release = releases.and_then(|items| {
                items
                    .iter()
                    .find(|item| {
                        !album.is_empty()
                            && item["title"]
                                .as_str()
                                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(album))
                    })
                    .or_else(|| items.first())
            });
            let media = release.and_then(|item| item["media"].as_array());
            let medium = media.and_then(|items| items.first());
            let track = medium
                .and_then(|item| {
                    item["tracks"]
                        .as_array()
                        .or_else(|| item["track"].as_array())
                })
                .and_then(|items| items.first());
            let year = release
                .and_then(|item| item["date"].as_str())
                .or_else(|| recording["first-release-date"].as_str())
                .unwrap_or_default()
                .chars()
                .take(4)
                .collect();
            let score = recording["score"]
                .as_str()
                .and_then(|value| value.parse().ok())
                .or_else(|| recording["score"].as_u64().map(|value| value as u8))
                .unwrap_or(0)
                .min(100);
            let release_artist = release
                .map(|item| musicbrainz_credit(&item["artist-credit"]))
                .unwrap_or_default();
            let compilation = release
                .and_then(|item| item["release-group"]["secondary-types"].as_array())
                .is_some_and(|items| {
                    items
                        .iter()
                        .any(|item| item.as_str() == Some("Compilation"))
                });
            candidates.push(MusicBrainzCandidate {
                id: recording["id"].as_str().unwrap_or_default().to_string(),
                score,
                title: recording["title"].as_str().unwrap_or_default().to_string(),
                artist: musicbrainz_credit(&recording["artist-credit"]),
                album: release
                    .and_then(|item| item["title"].as_str())
                    .unwrap_or_default()
                    .to_string(),
                album_artist: if release_artist.is_empty() {
                    musicbrainz_credit(&recording["artist-credit"])
                } else {
                    release_artist
                },
                year,
                track: track
                    .map(|item| {
                        item["number"]
                            .as_str()
                            .map(str::to_string)
                            .or_else(|| item["position"].as_u64().map(|value| value.to_string()))
                            .unwrap_or_default()
                    })
                    .unwrap_or_default(),
                track_total: medium
                    .and_then(|item| item["track-count"].as_u64())
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                disc: medium
                    .and_then(|item| item["position"].as_u64())
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                disc_total: media
                    .map(|items| items.len().to_string())
                    .unwrap_or_default(),
                compilation,
            });
        }
    }
    Ok(candidates)
}

#[tauri::command]
async fn search_album_covers(album: String, artist: String, discogs_token: Option<String>) -> Result<Vec<CoverSearchResult>, String> {
    let album = album.trim();
    let artist = artist.trim();
    if album.is_empty() { return Err("Gib einen Albumnamen ein.".into()); }
    let client = reqwest::Client::builder().user_agent("MetaMagic/0.1.0 (desktop music library)").timeout(Duration::from_secs(18)).build().map_err(|e| e.to_string())?;
    let mut results = Vec::new();
    let query = format!("{} {}", artist, album).trim().to_owned();

    // Apple Search API: collection artwork is returned directly and can be enlarged by replacing size suffix.
    let apple = client.get("https://itunes.apple.com/search/").query(&[("term", query.as_str()), ("entity", "album"), ("limit", "50")]).send().await;
    match apple {
        Ok(response) if response.status().is_success() => {
            if let Ok(json) = response.json::<serde_json::Value>().await {
                if let Some(items) = json["results"].as_array() {
                    for item in items {
                        let title = item["collectionName"].as_str().unwrap_or_default();
                        let item_artist = item["artistName"].as_str().unwrap_or_default();
                        let image = item["artworkUrl100"].as_str().unwrap_or_default();
                        if title.is_empty() || image.is_empty() { continue; }
                        let full = image.replace("100x100bb", "1200x1200bb").replace("100x100-75", "1200x1200-75");
                        results.push(CoverSearchResult { id: format!("itunes:{}", item["collectionId"].as_i64().unwrap_or(0)), source: "iTunes".into(), title: title.into(), artist: item_artist.into(), year: item["releaseDate"].as_str().unwrap_or_default().get(0..4).unwrap_or_default().into(), image_url: full, thumbnail_url: image.into() });
                    }
                }
            }
        },
        Ok(_) | Err(_) => {}
    }

    // Discogs Database API; public search works without credentials, though authenticated image access is more reliable.
    let mut discogs_request = client.get("https://api.discogs.com/database/search/").query(&[("q", query.as_str()), ("type", "release"), ("per_page", "50")]);
    if let Some(token) = discogs_token.as_deref().filter(|value| !value.trim().is_empty()) { discogs_request = discogs_request.query(&[("token", token.trim())]); }
    if let Ok(response) = discogs_request.send().await {
        if response.status().is_success() {
            if let Ok(json) = response.json::<serde_json::Value>().await {
                if let Some(items) = json["results"].as_array() {
                    for item in items {
                        let title = item["title"].as_str().unwrap_or_default();
                        let thumb = item["thumb"].as_str().filter(|url| !url.is_empty()).or_else(|| item["cover_image"].as_str()).unwrap_or_default();
                        let full = item["cover_image"].as_str().filter(|url| !url.is_empty()).unwrap_or(thumb);
                        if title.is_empty() || thumb.is_empty() { continue; }
                        let (item_artist, item_title) = title.split_once(" - ").map(|(a,t)|(a.to_owned(),t.to_owned())).unwrap_or_else(|| (String::new(), title.to_owned()));
                        results.push(CoverSearchResult { id: format!("discogs:{}", item["id"].as_i64().unwrap_or(0)), source: "Discogs".into(), title: item_title, artist: item_artist, year: item["year"].as_i64().map(|v| v.to_string()).or_else(|| item["year"].as_str().map(str::to_owned)).unwrap_or_default(), image_url: full.into(), thumbnail_url: thumb.into() });
                    }
                }
            }
        }
    }

    // MusicBrainz release groups + Cover Art Archive, preserving the app's polite request spacing.
    let escape = |value: &str| value.replace('\\', "\\\\").replace('"', "\\\"");
    let mut terms = vec![format!("releasegroup:\"{}\"", escape(album))];
    if !artist.is_empty() { terms.push(format!("artist:\"{}\"", escape(artist))); }
    let query = terms.join(" AND ");
    wait_for_musicbrainz_slot().await?;
    if let Ok(response) = client.get("https://musicbrainz.org/ws/2/release-group/").query(&[("query", query.as_str()), ("fmt", "json"), ("limit", "50")]).send().await {
        if response.status().is_success() {
            if let Ok(json) = response.json::<serde_json::Value>().await {
                if let Some(groups) = json["release-groups"].as_array() {
                    for group in groups {
                        let id = group["id"].as_str().unwrap_or_default();
                        if id.is_empty() { continue; }
                        let Ok(art_response) = client.get(format!("https://coverartarchive.org/release-group/{id}/")).send().await else { continue; };
                        if !art_response.status().is_success() { continue; }
                        let Ok(art) = art_response.json::<serde_json::Value>().await else { continue; };
                        let Some(image) = art["images"].as_array().and_then(|images| images.iter().find(|entry| entry["front"].as_bool()==Some(true)).or_else(|| images.first())) else { continue; };
                        let full = image["image"].as_str().unwrap_or_default();
                        let thumb = image["thumbnails"]["500"].as_str().or_else(|| image["thumbnails"]["250"].as_str()).or_else(|| image["thumbnails"]["small"].as_str()).unwrap_or(full);
                        if full.is_empty() { continue; }
                        results.push(CoverSearchResult { id: format!("musicbrainz:{id}"), source: "MusicBrainz".into(), title: group["title"].as_str().unwrap_or_default().into(), artist: musicbrainz_credit(&group["artist-credit"]), year: group["first-release-date"].as_str().unwrap_or_default().get(0..4).unwrap_or_default().into(), image_url: full.into(), thumbnail_url: thumb.into() });
                    }
                }
            }
        }
    }
    Ok(results)
}

#[tauri::command]
async fn apply_album_cover(app: tauri::AppHandle, paths: Vec<String>, image_url: String) -> Result<usize, String> {
    if paths.is_empty() { return Err("Es wurden keine Titel zum Aktualisieren angegeben.".into()); }
    let parsed = reqwest::Url::parse(&image_url).map_err(|e| format!("Ungültige Cover-Adresse: {e}"))?;
    if !matches!(parsed.scheme(), "https" | "http") { return Err("Das Cover muss über HTTP oder HTTPS erreichbar sein.".into()); }
    let client = reqwest::Client::builder().user_agent("MetaMagic/0.1.0 (desktop music library)").timeout(Duration::from_secs(25)).build().map_err(|e| e.to_string())?;
    let response = client.get(parsed).send().await.map_err(|e| format!("Coverbild konnte nicht geladen werden: {e}"))?.error_for_status().map_err(|e| format!("Coverbild konnte nicht geladen werden: {e}"))?;
    let mime = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|value| value.to_str().ok()).map(str::to_owned);
    let bytes = response.bytes().await.map_err(|e| format!("Coverbild konnte nicht gelesen werden: {e}"))?;
    if bytes.is_empty() || bytes.len() > 25 * 1024 * 1024 { return Err("Das Cover ist leer oder größer als 25 MB.".into()); }
    let mime = image_mime(&bytes, mime.as_deref());
    if !mime.starts_with("image/") { return Err("Die Quelle hat kein gültiges Bild geliefert.".into()); }
    let mut updated = 0;
    let mut errors = Vec::new();
    let mut successful_paths = Vec::new();
    for raw_path in paths {
        let path = PathBuf::from(&raw_path);
        let result = (|| -> Result<(), String> {
            let had_id3v2 = std::fs::read(&path).map(|file| file.starts_with(b"ID3")).unwrap_or(false);
            let (mut tagged, repaired_id3) = match read_tagged(&path) {
                Ok(tagged) => { let repaired = had_id3v2 && tagged.tag(lofty::tag::TagType::Id3v2).is_none(); (tagged, repaired) },
                Err(error) if FileType::from_path(&path) == Some(FileType::Mpeg) && error.contains("UTF-16 string has an invalid byte order mark") => {
                    let audio = std::fs::read(&path).map_err(|e| e.to_string())?;
                    let mut audio_without_id3 = audio;
                    if !strip_leading_id3v2(&mut audio_without_id3) { return Err(error); }
                    let tagged = Probe::with_file_type(Cursor::new(audio_without_id3), FileType::Mpeg).options(ParseOptions::new().read_tags(false).parsing_mode(ParsingMode::Relaxed)).read().map_err(|e| e.to_string())?;
                    (tagged, true)
                },
                Err(error) => return Err(error),
            };
            let tag_type = if FileType::from_path(&path) == Some(FileType::Mpeg) { lofty::tag::TagType::Id3v2 } else { tagged.primary_tag_type() };
            if tagged.tag(tag_type).is_none() { tagged.insert_tag(Tag::new(tag_type)); }
            let tag = tagged.tag_mut(tag_type).ok_or_else(|| "Datei besitzt keinen editierbaren Tag".to_string())?;
            let old = tag.pictures().iter().enumerate().filter_map(|(index, picture)| (picture.pic_type() == PictureType::CoverFront).then_some(index)).collect::<Vec<_>>();
            for index in old.into_iter().rev() { tag.remove_picture(index); }
            tag.push_picture(Picture::new_unchecked(PictureType::CoverFront, Some(MimeType::from_str(&mime)), None, bytes.to_vec()));
                    if repaired_id3 {
                let mut clean_audio = std::fs::read(&path).map_err(|e| e.to_string())?;
                if !strip_leading_id3v2(&mut clean_audio) { return Err("Beschädigter ID3-Tag konnte nicht sicher entfernt werden.".into()); }
                let temp_path = path.with_file_name(format!(".metamagic-cover-{}.tmp.mp3", std::process::id()));
                std::fs::write(&temp_path, clean_audio).map_err(|e| format!("Temporäre Reparaturdatei konnte nicht erstellt werden: {e}"))?;
                let save_result = tagged.save_to_path(&temp_path, WriteOptions::default()).map_err(|e| format!("Cover konnte nicht in die Reparaturdatei geschrieben werden: {e}"));
                if save_result.is_ok() {
                    let verify = verify_embedded_front_cover(&temp_path);
                    match verify { Ok(()) => { let replacement = path.with_file_name(format!(".metamagic-audio-{}.tmp", std::process::id())); let copied = std::fs::copy(&temp_path, &replacement).map_err(|e| format!("Reparierte Datei konnte nicht vorbereitet werden: {e}")); let _ = std::fs::remove_file(&temp_path); copied?; std::fs::rename(&replacement, &path).map_err(|e| { let _=std::fs::remove_file(&replacement); format!("Reparierte Datei konnte nicht übernommen werden: {e}") })?; verify_embedded_front_cover(&path) }, Err(e) => { let _ = std::fs::remove_file(&temp_path); Err(e) } }
                } else { let _ = std::fs::remove_file(&temp_path); save_result }
            } else {
                tagged.save_to_path(&path, write_options_for(&path)).map_err(|e| format!("Tag konnte nicht gespeichert werden: {e}"))?;
                verify_embedded_front_cover(&path)?;
                Ok(())
            }
        })();
        match result { Ok(()) => { updated += 1; successful_paths.push(raw_path.clone()); }, Err(error) => errors.push(format!("{}: {error}", path.file_name().and_then(|v|v.to_str()).unwrap_or(&raw_path))) }
    }
    if updated == 0 { return Err(if errors.is_empty() { "Kein Titel konnte aktualisiert werden.".into() } else { errors.join("\n") }); }
    if updated > 0 {
        let db = database(&app)?;
        for raw_path in &successful_paths {
            db.execute("INSERT OR REPLACE INTO track_artwork_cache(path,mime,image,updated_at) VALUES (?1,?2,?3,CURRENT_TIMESTAMP)", params![raw_path,mime,bytes.as_ref()]).map_err(|e| format!("Cover wurde eingebettet, konnte aber nicht für die Anzeige gespeichert werden: {e}"))?;
        }
    }
    if errors.is_empty() { Ok(updated) } else { Err(format!("{updated} Titel aktualisiert. Fehler bei {} Datei(en):\n{}", errors.len(), errors.join("\n")) ) }
}

fn verify_embedded_front_cover(path: &Path) -> Result<(), String> {
    let raw = std::fs::read(path).map_err(|e| e.to_string())?;
    if id3_picture_frames(&raw).iter().any(|(kind, _, data)| *kind == PictureType::CoverFront && !data.is_empty()) { return Ok(()); }
    let tagged = read_tagged(path).map_err(|error| format!("Reparierte Datei konnte nicht erneut gelesen werden: {error}"))?;
    if tagged.tags().iter().any(|tag| tag.get_picture_type(PictureType::CoverFront).is_some_and(|picture| !picture.data().is_empty())) { return Ok(()); }
    Err("Die gespeicherte MP3 enthält keinen ID3v2-Bildframe (APIC).".into())
}

fn send_scan_progress(
    channel: &Channel<ScanProgress>,
    phase: &str,
    current: usize,
    total: Option<usize>,
    succeeded: usize,
    failed: usize,
    current_file: String,
) {
    let _ = channel.send(ScanProgress {
        phase: phase.to_string(),
        current,
        total,
        succeeded,
        failed,
        current_file,
    });
}

fn scan_files_blocking(
    app: tauri::AppHandle,
    paths: Vec<String>,
    on_progress: Channel<ScanProgress>,
) -> Result<ScanSummary, String> {
    let mut db = database(&app)?;
    // Files selected through the dialog are readable by Rust, but the WebView's
    // asset:// protocol has a separate allowlist. Grant only selected files/folders.
    let asset_scope = app.asset_protocol_scope();
    {
        let db = database(&app)?;
        for raw in &paths {
            let root = PathBuf::from(raw);
            if root.is_dir() {
                db.execute("INSERT OR IGNORE INTO library_folders(path) VALUES (?1)", [root.to_string_lossy().as_ref()]).map_err(|e| e.to_string())?;
            }
        }
    }
    for raw in &paths {
        let root = PathBuf::from(raw);
        if root.is_file() {
            asset_scope
                .allow_file(&root)
                .map_err(|e| format!("Datei konnte für die Wiedergabe freigegeben werden: {e}"))?;
        } else if root.is_dir() {
            asset_scope.allow_directory(&root, true).map_err(|e| {
                format!("Musikordner konnte für die Wiedergabe freigegeben werden: {e}")
            })?;
        }
    }

    let mut files = Vec::<PathBuf>::new();
    let mut visited = 0usize;
    let mut discovery_failed = 0usize;
    let mut last_discovery_update = Instant::now();
    for raw in paths {
        let root = PathBuf::from(raw);
        if root.is_file() {
            files.push(root);
        } else if root.is_dir() {
            for result in WalkDir::new(root).follow_links(false) {
                visited += 1;
                let current_file = match result {
                    Ok(entry) => {
                        let path = entry.path();
                        let current_file = path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        if entry.file_type().is_file() && supported(path) {
                            files.push(path.to_path_buf());
                        }
                        current_file
                    }
                    Err(error) => {
                        discovery_failed += 1;
                        error
                            .path()
                            .and_then(Path::file_name)
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    }
                };
                if visited % 500 == 0 || last_discovery_update.elapsed().as_millis() >= 250 {
                    send_scan_progress(
                        &on_progress,
                        "discover",
                        visited,
                        None,
                        files.len(),
                        discovery_failed,
                        current_file,
                    );
                    last_discovery_update = Instant::now();
                }
            }
        }
    }
    let total = files.len();
    send_scan_progress(
        &on_progress,
        "read",
        0,
        Some(total),
        0,
        discovery_failed,
        String::new(),
    );

    let tx = db.transaction().map_err(|e| e.to_string())?;
    let mut succeeded = 0usize;
    let mut failed = discovery_failed;
    let mut last_read_update = Instant::now();
    for (index, path) in files.iter().enumerate() {
        if let Ok(track) = read_one(path) {
            tx.execute("INSERT INTO tracks (path,title,artist,album,album_artist,year,genre,track_no,disc_no,duration_seconds,bitrate,sample_rate,channels,bit_depth,format,extra_tags_json,track_total,disc_total,compilation) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19) ON CONFLICT(path) DO UPDATE SET title=excluded.title,artist=excluded.artist,album=excluded.album,album_artist=excluded.album_artist,year=excluded.year,genre=excluded.genre,track_no=excluded.track_no,disc_no=excluded.disc_no,duration_seconds=excluded.duration_seconds,bitrate=excluded.bitrate,sample_rate=excluded.sample_rate,channels=excluded.channels,bit_depth=excluded.bit_depth,format=excluded.format,extra_tags_json=excluded.extra_tags_json,track_total=excluded.track_total,disc_total=excluded.disc_total,compilation=excluded.compilation",
                params![track.path,track.title,track.artist,track.album,track.album_artist,track.year,track.genre,track.track,track.disc,track.duration_seconds as i64,track.bitrate,track.sample_rate,track.channels,track.bit_depth,track.format,serde_json::to_string(&track.extra_tags).map_err(|e|e.to_string())?,track.track_total,track.disc_total,track.compilation as i64]).map_err(|e| e.to_string())?;
            succeeded += 1;
        } else {
            failed += 1;
        }
        let processed = index + 1;
        if processed % 250 == 0
            || last_read_update.elapsed().as_millis() >= 200
            || processed == total
        {
            send_scan_progress(
                &on_progress,
                "read",
                processed,
                Some(total),
                succeeded,
                failed,
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            );
            last_read_update = Instant::now();
        }
    }
    send_scan_progress(
        &on_progress,
        "saving",
        total,
        Some(total),
        succeeded,
        failed,
        String::new(),
    );
    tx.commit().map_err(|e| e.to_string())?;
    send_scan_progress(
        &on_progress,
        "done",
        total,
        Some(total),
        succeeded,
        failed,
        String::new(),
    );
    Ok(ScanSummary {
        discovered: total,
        imported: succeeded,
        failed,
    })
}

#[tauri::command]
async fn scan_files(
    app: tauri::AppHandle,
    paths: Vec<String>,
    on_progress: Channel<ScanProgress>,
) -> Result<ScanSummary, String> {
    tauri::async_runtime::spawn_blocking(move || scan_files_blocking(app, paths, on_progress))
        .await
        .map_err(|e| format!("Import konnte nicht gestartet werden: {e}"))?
}

#[tauri::command]
fn list_tracks(app: tauri::AppHandle) -> Result<Vec<Track>, String> {
    let db = database(&app)?;
    let mut stmt = db.prepare("SELECT path,title,artist,album,album_artist,year,genre,track_no,disc_no,duration_seconds,bitrate,sample_rate,channels,bit_depth,format,extra_tags_json,track_total,disc_total,compilation FROM tracks ORDER BY artist,album,track_no,title").map_err(|e|e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            let extras: String = r.get(15)?;
            Ok(Track {
                path: r.get(0)?,
                title: r.get(1)?,
                artist: r.get(2)?,
                album: r.get(3)?,
                album_artist: r.get(4)?,
                year: r.get(5)?,
                genre: r.get(6)?,
                track: r.get(7)?,
                disc: r.get(8)?,
                duration_seconds: r.get::<_, i64>(9)?.max(0) as u64,
                bitrate: r.get(10)?,
                sample_rate: r.get(11)?,
                channels: r.get(12)?,
                bit_depth: r.get(13)?,
                format: r.get(14)?,
                extra_tags: serde_json::from_str(&extras).unwrap_or_default(),
                track_total: r.get(16)?,
                disc_total: r.get(17)?,
                compilation: r.get::<_, i64>(18)? != 0,
            })
        })
        .map_err(|e| e.to_string())?;
    let tracks = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    // Migrate libraries imported before asset-protocol permissions were granted.
    // Non-recursive parent access covers the already indexed files in each folder.
    let asset_scope = app.asset_protocol_scope();
    let mut parent_dirs = std::collections::HashSet::new();
    for track in &tracks {
        if asset_scope.is_allowed(&track.path) {
            continue;
        }
        if let Some(parent) = Path::new(&track.path).parent() {
            parent_dirs.insert(parent.to_path_buf());
        } else {
            let _ = asset_scope.allow_file(&track.path);
        }
    }
    for parent in parent_dirs {
        let _ = asset_scope.allow_directory(parent, false);
    }
    Ok(tracks)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LibraryFolder {
    path: String,
    name: String,
    track_count: usize,
}

#[tauri::command]
fn list_library_folders(app: tauri::AppHandle) -> Result<Vec<LibraryFolder>, String> {
    let db = database(&app)?;
    let mut folders_stmt = db.prepare("SELECT path FROM library_folders ORDER BY path COLLATE NOCASE").map_err(|e| e.to_string())?;
    let folders = folders_stmt.query_map([], |row| row.get::<_, String>(0)).map_err(|e| e.to_string())?.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
    let mut tracks_stmt = db.prepare("SELECT path FROM tracks").map_err(|e| e.to_string())?;
    let tracks = tracks_stmt.query_map([], |row| row.get::<_, String>(0)).map_err(|e| e.to_string())?.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
    Ok(folders.into_iter().map(|path| {
        let root = Path::new(&path);
        let name = root.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let track_count = tracks.iter().filter(|track| Path::new(track).starts_with(root)).count();
        LibraryFolder { path, name, track_count }
    }).collect())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FilesystemEntry { path: String, name: String, is_directory: bool }

#[tauri::command]
fn browse_filesystem(path: String) -> Result<Vec<FilesystemEntry>, String> {
    let root = PathBuf::from(path);
    let entries = std::fs::read_dir(&root).map_err(|e| format!("Ordner kann nicht gelesen werden: {e}"))?;
    let mut result = Vec::new();
    for entry in entries {
        let entry = match entry { Ok(value) => value, Err(_) => continue };
        let file_type = match entry.file_type() { Ok(value) => value, Err(_) => continue };
        if file_type.is_symlink() { continue; }
        let path = entry.path();
        let is_directory = file_type.is_dir();
        if !is_directory && !supported(&path) { continue; }
        result.push(FilesystemEntry { path: path.to_string_lossy().into_owned(), name: entry.file_name().to_string_lossy().into_owned(), is_directory });
    }
    result.sort_by(|a,b| b.is_directory.cmp(&a.is_directory).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(result)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Playlist {
    id: i64,
    name: String,
    track_count: u32,
}

#[tauri::command]
fn list_playlists(app: tauri::AppHandle) -> Result<Vec<Playlist>, String> {
    let db = database(&app)?;
    let mut stmt=db.prepare("SELECT p.id,p.name,COUNT(pt.path) FROM playlists p LEFT JOIN playlist_tracks pt ON pt.playlist_id=p.id GROUP BY p.id ORDER BY p.name COLLATE NOCASE").map_err(|e|e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Playlist {
                id: r.get(0)?,
                name: r.get(1)?,
                track_count: r.get(2)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_favorites(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    let db = database(&app)?;
    let mut stmt = db
        .prepare("SELECT path FROM favorites ORDER BY added_at DESC,path")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn remove_tracks_from_library(app: tauri::AppHandle, paths: Vec<String>) -> Result<usize, String> {
    let mut db = database(&app)?;
    let tx = db.transaction().map_err(|e| e.to_string())?;
    let mut removed = 0;
    for path in paths {
        removed += tx
            .execute("DELETE FROM tracks WHERE path=?1", params![path])
            .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(removed)
}

#[tauri::command]
fn reveal_file(path: String) -> Result<(), String> {
    let path = PathBuf::from(path);
    if !path.is_file() {
        return Err("Die Datei ist nicht mehr vorhanden".into());
    }
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg("-R").arg(&path).status();
    #[cfg(target_os = "windows")]
    let result = Command::new("explorer.exe")
        .arg("/select,")
        .arg(&path)
        .status();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = Command::new("xdg-open")
        .arg(path.parent().unwrap_or_else(|| Path::new(".")))
        .status();
    let status = result.map_err(|e| format!("Dateimanager konnte nicht geöffnet werden: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("Dateimanager konnte die Datei nicht anzeigen".into())
    }
}

#[tauri::command]
fn set_favorite(app: tauri::AppHandle, path: String, favorite: bool) -> Result<(), String> {
    let db = database(&app)?;
    if favorite {
        let inserted = db.execute("INSERT OR IGNORE INTO favorites(path) SELECT ?1 WHERE EXISTS(SELECT 1 FROM tracks WHERE path=?1)", params![path]).map_err(|e| e.to_string())?;
        let exists: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM favorites WHERE path=?1)",
                params![path],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if !exists {
            return Err(if inserted == 0 {
                "Titel ist nicht in der Bibliothek".into()
            } else {
                "Favorit konnte nicht gespeichert werden".into()
            });
        }
    } else {
        db.execute("DELETE FROM favorites WHERE path=?1", params![path])
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn get_playlist_tracks(app: tauri::AppHandle, playlist_id: i64) -> Result<Vec<Track>, String> {
    let db = database(&app)?;
    let mut stmt=db.prepare("SELECT t.path,t.title,t.artist,t.album,t.album_artist,t.year,t.genre,t.track_no,t.disc_no,t.duration_seconds,t.bitrate,t.sample_rate,t.channels,t.bit_depth,t.format,t.extra_tags_json,t.track_total,t.disc_total,t.compilation FROM tracks t JOIN playlist_tracks pt ON pt.path=t.path WHERE pt.playlist_id=?1 ORDER BY pt.position").map_err(|e|e.to_string())?;
    let rows = stmt
        .query_map(params![playlist_id], |r| {
            let extras: String = r.get(15)?;
            Ok(Track {
                path: r.get(0)?,
                title: r.get(1)?,
                artist: r.get(2)?,
                album: r.get(3)?,
                album_artist: r.get(4)?,
                year: r.get(5)?,
                genre: r.get(6)?,
                track: r.get(7)?,
                disc: r.get(8)?,
                duration_seconds: r.get::<_, i64>(9)?.max(0) as u64,
                bitrate: r.get(10)?,
                sample_rate: r.get(11)?,
                channels: r.get(12)?,
                bit_depth: r.get(13)?,
                format: r.get(14)?,
                extra_tags: serde_json::from_str(&extras).unwrap_or_default(),
                track_total: r.get(16)?,
                disc_total: r.get(17)?,
                compilation: r.get::<_, i64>(18)? != 0,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn remove_track_from_playlist(
    app: tauri::AppHandle,
    playlist_id: i64,
    path: String,
) -> Result<(), String> {
    let mut db = database(&app)?;
    let tx = db.transaction().map_err(|e| e.to_string())?;
    let position: Option<i64> = tx
        .query_row(
            "SELECT position FROM playlist_tracks WHERE playlist_id=?1 AND path=?2",
            params![playlist_id, path],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM playlist_tracks WHERE playlist_id=?1 AND path=?2",
        params![playlist_id, path],
    )
    .map_err(|e| e.to_string())?;
    if let Some(position) = position {
        tx.execute(
            "UPDATE playlist_tracks SET position=position-1 WHERE playlist_id=?1 AND position>?2",
            params![playlist_id, position],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn remove_tracks_from_playlist(
    app: tauri::AppHandle,
    playlist_id: i64,
    paths: Vec<String>,
) -> Result<u32, String> {
    let mut db = database(&app)?;
    let tx = db.transaction().map_err(|e| e.to_string())?;
    let mut removed = 0u32;
    for path in paths {
        let position: Option<i64> = tx
            .query_row(
                "SELECT position FROM playlist_tracks WHERE playlist_id=?1 AND path=?2",
                params![playlist_id, path],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(position) = position {
            tx.execute(
                "DELETE FROM playlist_tracks WHERE playlist_id=?1 AND path=?2",
                params![playlist_id, path],
            )
            .map_err(|e| e.to_string())?;
            tx.execute("UPDATE playlist_tracks SET position=position-1 WHERE playlist_id=?1 AND position>?2",params![playlist_id,position]).map_err(|e| e.to_string())?;
            removed += 1;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(removed)
}

#[tauri::command]
fn reorder_playlist_tracks(
    app: tauri::AppHandle,
    playlist_id: i64,
    paths: Vec<String>,
) -> Result<(), String> {
    let mut db = database(&app)?;
    let tx = db.transaction().map_err(|e| e.to_string())?;
    for (index, path) in paths.iter().enumerate() {
        tx.execute(
            "UPDATE playlist_tracks SET position=?3 WHERE playlist_id=?1 AND path=?2",
            params![playlist_id, path, index as i64],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn create_playlist(app: tauri::AppHandle, name: String) -> Result<Playlist, String> {
    let clean = name.trim();
    if clean.is_empty() {
        return Err("Playlistname darf nicht leer sein".into());
    }
    let db = database(&app)?;
    db.execute("INSERT INTO playlists(name) VALUES (?1)", params![clean])
        .map_err(|e| e.to_string())?;
    Ok(Playlist {
        id: db.last_insert_rowid(),
        name: clean.to_owned(),
        track_count: 0,
    })
}

#[tauri::command]
fn rename_playlist(
    app: tauri::AppHandle,
    playlist_id: i64,
    name: String,
) -> Result<String, String> {
    let clean = name.trim();
    if clean.is_empty() {
        return Err("Playlistname darf nicht leer sein".into());
    }
    let db = database(&app)?;
    let changed = db
        .execute(
            "UPDATE playlists SET name=?2 WHERE id=?1",
            params![playlist_id, clean],
        )
        .map_err(|e| e.to_string())?;
    if changed == 0 {
        return Err("Playlist wurde nicht gefunden".into());
    }
    Ok(clean.to_owned())
}

#[tauri::command]
fn delete_playlist(app: tauri::AppHandle, playlist_id: i64) -> Result<(), String> {
    let db = database(&app)?;
    let changed = db
        .execute("DELETE FROM playlists WHERE id=?1", params![playlist_id])
        .map_err(|e| e.to_string())?;
    if changed == 0 {
        return Err("Playlist wurde nicht gefunden".into());
    }
    Ok(())
}

#[tauri::command]
fn add_tracks_to_playlist(
    app: tauri::AppHandle,
    playlist_id: i64,
    paths: Vec<String>,
) -> Result<u32, String> {
    let mut db = database(&app)?;
    let tx = db.transaction().map_err(|e| e.to_string())?;
    let mut position: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(position)+1,0) FROM playlist_tracks WHERE playlist_id=?1",
            params![playlist_id],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    let mut added = 0u32;
    for path in paths {
        let inserted=tx.execute("INSERT OR IGNORE INTO playlist_tracks(playlist_id,path,position) SELECT ?1,?2,?3 WHERE EXISTS(SELECT 1 FROM tracks WHERE path=?2)",params![playlist_id,path,position]).map_err(|e|e.to_string())?;
        if inserted > 0 {
            position += 1;
            added += 1;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(added)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TagChanges {
    pub path: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub year: Option<String>,
    pub genre: Option<String>,
    pub track: Option<String>,
    pub disc: Option<String>,
    pub track_total: Option<String>,
    pub disc_total: Option<String>,
    pub compilation: Option<bool>,
    pub extra_tags: Option<BTreeMap<String, String>>,
}

fn extra_item_key(name: &str) -> Option<ItemKey> {
    Some(match name {
        "composer" => ItemKey::Composer,
        "work" => ItemKey::Work,
        "movement" => ItemKey::Movement,
        "movementNumber" => ItemKey::MovementNumber,
        "movementTotal" => ItemKey::MovementTotal,
        "grouping" => ItemKey::ContentGroup,
        "lyrics" => ItemKey::Lyrics,
        "comments" => ItemKey::Comment,
        "bpm" => ItemKey::IntegerBpm,
        "key" => ItemKey::InitialKey,
        "mood" => ItemKey::Mood,
        "remixer" => ItemKey::Remixer,
        "producer" => ItemKey::Producer,
        "publisher" => ItemKey::Publisher,
        "label" => ItemKey::Label,
        "isrc" => ItemKey::Isrc,
        "barcode" => ItemKey::Barcode,
        "catalogNumber" => ItemKey::CatalogNumber,
        "releaseDate" => ItemKey::ReleaseDate,
        "originalReleaseDate" => ItemKey::OriginalReleaseDate,
        "copyright" => ItemKey::CopyrightMessage,
        "language" => ItemKey::Language,
        "encodedBy" => ItemKey::EncodedBy,
        _ => return None,
    })
}

fn apply_totals(
    tag: &mut Tag,
    track_total: Option<&str>,
    disc_total: Option<&str>,
) -> Result<(), String> {
    if let Some(value) = track_total {
        if value.trim().is_empty() {
            tag.remove_track_total()
        } else {
            tag.set_track_total(
                value
                    .trim()
                    .parse()
                    .map_err(|_| "Track-Gesamtzahl muss eine positive ganze Zahl sein")?,
            );
        }
    }
    if let Some(value) = disc_total {
        if value.trim().is_empty() {
            tag.remove_disk_total()
        } else {
            tag.set_disk_total(
                value
                    .trim()
                    .parse()
                    .map_err(|_| "CD-Gesamtzahl muss eine positive ganze Zahl sein")?,
            );
        }
    }
    Ok(())
}

fn apply_snapshot(path: &Path, snapshot: &Track) -> Result<(), String> {
    let mut tagged = read_tagged(path)?;
    let tag_type = tagged.primary_tag_type();
    if tagged.tag(tag_type).is_none() {
        tagged.insert_tag(Tag::new(tag_type));
    }
    let tag = tagged
        .tag_mut(tag_type)
        .ok_or_else(|| "Dateiformat enthält keinen beschreibbaren Tag".to_string())?;
    if snapshot.title.is_empty() {
        tag.remove_title()
    } else {
        tag.set_title(snapshot.title.clone())
    }
    if snapshot.artist.is_empty() {
        tag.remove_artist()
    } else {
        tag.set_artist(snapshot.artist.clone())
    }
    if snapshot.album.is_empty() {
        tag.remove_album()
    } else {
        tag.set_album(snapshot.album.clone())
    }
    if snapshot.album_artist.is_empty() {
        tag.remove_key(&ItemKey::AlbumArtist);
    } else {
        tag.insert_text(ItemKey::AlbumArtist, snapshot.album_artist.clone());
    }
    if let Some(v) = snapshot.year {
        tag.set_year(v)
    } else {
        tag.remove_year()
    }
    if snapshot.genre.is_empty() {
        tag.remove_genre()
    } else {
        tag.set_genre(snapshot.genre.clone())
    }
    if let Some(v) = snapshot.track {
        tag.set_track(v)
    } else {
        tag.remove_track()
    }
    if let Some(v) = snapshot.disc {
        tag.set_disk(v)
    } else {
        tag.remove_disk()
    }
    if let Some(v) = snapshot.track_total {
        tag.set_track_total(v)
    } else {
        tag.remove_track_total()
    }
    if let Some(v) = snapshot.disc_total {
        tag.set_disk_total(v)
    } else {
        tag.remove_disk_total()
    }
    if snapshot.compilation {
        tag.insert_text(ItemKey::FlagCompilation, "1".into());
    } else {
        tag.remove_key(&ItemKey::FlagCompilation);
    }
    for (name, value) in &snapshot.extra_tags {
        if let Some(key) = extra_item_key(name) {
            if value.is_empty() {
                tag.remove_key(&key);
            } else {
                tag.insert_text(key, value.clone());
            }
        }
    }
    tagged
        .save_to_path(path, write_options_for(path))
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn write_tags(
    app: tauri::AppHandle,
    changes: Vec<TagChanges>,
) -> Vec<(String, Result<(), String>)> {
    let db = database(&app).ok();
    let mut snapshots: Vec<(Track, Track)> = Vec::new();
    let results:Vec<(String,Result<(),String>)>=changes.into_iter().map(|change| {
        let path = PathBuf::from(&change.path);
        let before=read_one(&path).ok();
        let result = (|| {
            let mut tagged = read_tagged(&path)?;
            let tag_type = tagged.primary_tag_type();
            if tagged.tag(tag_type).is_none() { tagged.insert_tag(Tag::new(tag_type)); }
            let tag = tagged.tag_mut(tag_type).ok_or_else(|| "Dateiformat enthält keinen beschreibbaren Tag".to_string())?;
            if let Some(v)=change.title {if v.is_empty(){tag.remove_title()}else{tag.set_title(v)}} if let Some(v)=change.artist {if v.is_empty(){tag.remove_artist()}else{tag.set_artist(v)}}
            if let Some(v)=change.album {if v.is_empty(){tag.remove_album()}else{tag.set_album(v)}} if let Some(v)=change.album_artist {if v.is_empty(){tag.remove_key(&ItemKey::AlbumArtist);}else{tag.insert_text(ItemKey::AlbumArtist,v);}}
            if let Some(v)=change.year {if v.trim().is_empty(){tag.remove_year()}else{tag.set_year(v.trim().parse().map_err(|_|"Jahr muss aus einer gültigen Jahreszahl bestehen")?);}} if let Some(v)=change.genre {if v.is_empty(){tag.remove_genre()}else{tag.set_genre(v)}}
            if let Some(v)=change.track {if v.trim().is_empty(){tag.remove_track()}else{tag.set_track(v.trim().parse().map_err(|_|"Tracknummer muss eine positive ganze Zahl sein")?);}}
            if let Some(v)=change.disc {if v.trim().is_empty(){tag.remove_disk()}else{tag.set_disk(v.trim().parse().map_err(|_|"Discnummer muss eine positive ganze Zahl sein")?);}}
            apply_totals(tag,change.track_total.as_deref(),change.disc_total.as_deref())?;
            if let Some(compilation)=change.compilation{if compilation{tag.insert_text(ItemKey::FlagCompilation,"1".into());}else{tag.remove_key(&ItemKey::FlagCompilation);}}
            if let Some(extra_tags)=change.extra_tags{for (name,value) in extra_tags{let key=extra_item_key(&name).ok_or_else(||format!("Unbekanntes Tag-Feld: {name}"))?;if value.is_empty(){tag.remove_key(&key);}else{tag.insert_text(key,value);}}}
            tagged.save_to_path(&path, write_options_for(&path)).map_err(|e| e.to_string())?;
            Ok(())
        })();
        if result.is_ok() {
            if let (Some(db), Ok(updated)) = (db.as_ref(), read_one(&path)) {
                let extras=serde_json::to_string(&updated.extra_tags).unwrap_or_else(|_|"{}".into());
                let _ = db.execute("UPDATE tracks SET title=?2,artist=?3,album=?4,album_artist=?5,year=?6,genre=?7,track_no=?8,disc_no=?9,duration_seconds=?10,bitrate=?11,sample_rate=?12,format=?13,extra_tags_json=?14,track_total=?15,disc_total=?16,compilation=?17 WHERE path=?1",
                    params![updated.path,updated.title,updated.artist,updated.album,updated.album_artist,updated.year,updated.genre,updated.track,updated.disc,updated.duration_seconds as i64,updated.bitrate,updated.sample_rate,updated.format,extras,updated.track_total,updated.disc_total,updated.compilation as i64]);
                if let Some(before)=before {snapshots.push((before,updated));}
            }
        }
        (change.path, result)
    }).collect();
    if !snapshots.is_empty() {
        if let Some(db) = db.as_ref() {
            let _ = save_history(db, &snapshots);
        }
    }
    results
}

fn save_history(db: &Connection, snapshots: &[(Track, Track)]) -> Result<(), String> {
    let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
    tx.execute("DELETE FROM tag_history WHERE applied=0", [])
        .map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO tag_history(summary,applied) VALUES (?1,1)",
        params![format!("{} Datei(en) bearbeitet", snapshots.len())],
    )
    .map_err(|e| e.to_string())?;
    let id = tx.last_insert_rowid();
    for (before, after) in snapshots {
        tx.execute("INSERT INTO tag_history_items(history_id,path,before_json,after_json) VALUES (?1,?2,?3,?4)",params![id,before.path,serde_json::to_string(before).map_err(|e|e.to_string())?,serde_json::to_string(after).map_err(|e|e.to_string())?]).map_err(|e|e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoryEntry {
    id: i64,
    created_at: String,
    summary: String,
    applied: bool,
    kind: String,
}

#[tauri::command]
fn list_tag_history(app: tauri::AppHandle) -> Result<Vec<HistoryEntry>, String> {
    let db = database(&app)?;
    let mut stmt = db
        .prepare(
            "SELECT id,created_at,summary,applied,kind FROM tag_history ORDER BY id DESC LIMIT 30",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(HistoryEntry {
                id: r.get(0)?,
                created_at: r.get(1)?,
                summary: r.get(2)?,
                applied: r.get::<_, i64>(3)? != 0,
                kind: r.get(4)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

fn set_history_state(app: &tauri::AppHandle, redo: bool) -> Result<String, String> {
    let mut db = database(app)?;
    let tx = db.transaction().map_err(|e| e.to_string())?;
    let query = if redo {
        "SELECT id FROM tag_history WHERE applied=0 ORDER BY id ASC LIMIT 1"
    } else {
        "SELECT id FROM tag_history WHERE applied=1 ORDER BY id DESC LIMIT 1"
    };
    let row: Option<(i64, String)> = tx
        .query_row(&query, [], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .map_err(|e| e.to_string())?;
    let (id, kind) = row.ok_or_else(|| {
        if redo {
            "Nichts zum Wiederherstellen".to_string()
        } else {
            "Nichts zum Rückgängigmachen".to_string()
        }
    })?;
    let column = if redo { "after_json" } else { "before_json" };
    let sql = format!(
        "SELECT path,{column},{} FROM tag_history_items WHERE history_id=?1 ORDER BY path",
        if redo { "before_json" } else { "after_json" }
    );
    let items: Vec<(String, String, String)> = {
        let mut stmt = tx.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
    };
    let moves: Vec<(String, String)> = {
        let mut stmt=tx.prepare("SELECT old_path,new_path FROM tag_history_moves WHERE history_id=?1 ORDER BY old_path").map_err(|e|e.to_string())?;
        let rows = stmt
            .query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
    };
    let mut moved: Vec<(String, String)> = Vec::new();
    for (old, new) in &moves {
        let (source, target) = if redo { (old, new) } else { (new, old) };
        if !Path::new(source).is_file() {
            for (from, to) in moved.iter().rev() {
                let _ = std::fs::rename(to, from);
            }
            return Err(format!("Quelldatei fehlt: {source}"));
        }
        if Path::new(target).exists() {
            for (from, to) in moved.iter().rev() {
                let _ = std::fs::rename(to, from);
            }
            return Err(format!("Zielpfad ist bereits belegt: {target}"));
        }
        if let Err(e) = std::fs::rename(source, target) {
            for (from, to) in moved.iter().rev() {
                let _ = std::fs::rename(to, from);
            }
            return Err(format!("Datei konnte nicht verschoben werden: {e}"));
        }
        moved.push((source.clone(), target.clone()));
        let playlist_rows: Vec<(i64, i64)> = {
            let mut stmt = tx
                .prepare("SELECT playlist_id,position FROM playlist_tracks WHERE path=?1")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![source], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(|e| e.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?
        };
        tx.execute("DELETE FROM playlist_tracks WHERE path=?1", params![source])
            .map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE tracks SET path=?2 WHERE path=?1",
            params![source, target],
        )
        .map_err(|e| e.to_string())?;
        for (playlist_id, position) in playlist_rows {
            tx.execute(
                "INSERT INTO playlist_tracks(playlist_id,path,position) VALUES (?1,?2,?3)",
                params![playlist_id, target, position],
            )
            .map_err(|e| e.to_string())?;
        }
    }
    let mut done: Vec<(String, Track)> = Vec::new();
    for (path, target_json, rollback_json) in items {
        let target: Track = serde_json::from_str(&target_json).map_err(|e| e.to_string())?;
        let rollback: Track = serde_json::from_str(&rollback_json).map_err(|e| e.to_string())?;
        if let Err(error) = apply_snapshot(Path::new(&path), &target) {
            for (done_path, old) in done.iter().rev() {
                let _ = apply_snapshot(Path::new(done_path), old);
            }
            return Err(format!("Verlauf konnte nicht vollständig angewendet werden; bereits geänderte Dateien wurden zurückgesetzt: {error}"));
        }
        done.push((path, rollback));
    }
    let next_state = if redo { 1 } else { 0 };
    tx.execute(
        "UPDATE tag_history SET applied=?2 WHERE id=?1",
        params![id, next_state],
    )
    .map_err(|e| e.to_string())?;
    for (path, _) in &done {
        if let Ok(t) = read_one(Path::new(path)) {
            let extras = serde_json::to_string(&t.extra_tags).unwrap_or_else(|_| "{}".into());
            tx.execute("UPDATE tracks SET title=?2,artist=?3,album=?4,album_artist=?5,year=?6,genre=?7,track_no=?8,disc_no=?9,duration_seconds=?10,bitrate=?11,sample_rate=?12,format=?13,extra_tags_json=?14,track_total=?15,disc_total=?16,compilation=?17 WHERE path=?1",params![t.path,t.title,t.artist,t.album,t.album_artist,t.year,t.genre,t.track,t.disc,t.duration_seconds as i64,t.bitrate,t.sample_rate,t.format,extras,t.track_total,t.disc_total,t.compilation as i64]).map_err(|e|e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(format!(
        "{} wurde {}",
        if kind == "rename" {
            "Umbenennung"
        } else {
            "Tag-Änderung"
        },
        if redo {
            "wiederhergestellt"
        } else {
            "rückgängig gemacht"
        }
    ))
}

#[tauri::command]
fn undo_tag_changes(app: tauri::AppHandle) -> Result<String, String> {
    set_history_state(&app, false)
}
#[tauri::command]
fn redo_tag_changes(app: tauri::AppHandle) -> Result<String, String> {
    set_history_state(&app, true)
}

#[tauri::command]
fn inspect_file(path: String) -> Result<Track, String> {
    read_one(Path::new(&path))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BpmAnalysis {
    bpm: f64,
    confidence: f64,
    beat_offset_seconds: f64,
    key: String,
    key_name: String,
    camelot: String,
    key_confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackAnalysis {
    path: String,
    bpm: f64,
    confidence: f64,
    key: String,
    key_name: String,
    camelot: String,
    key_confidence: f64,
    beat_offset_seconds: f64,
}

#[tauri::command]
fn save_track_analyses(app: tauri::AppHandle, analyses: Vec<TrackAnalysis>) -> Result<(), String> {
    let mut db = database(&app)?;
    let tx = db.transaction().map_err(|e| e.to_string())?;
    for item in analyses {
        if !item.bpm.is_finite()
            || !(50.0..=220.0).contains(&item.bpm)
            || !item.beat_offset_seconds.is_finite()
            || !(0.0..=5.0).contains(&item.beat_offset_seconds)
            || !item.confidence.is_finite()
            || !(0.0..=1.0).contains(&item.confidence)
            || !item.key_confidence.is_finite()
            || !(0.0..=1.0).contains(&item.key_confidence)
        {
            return Err(format!("Ungültige Analysewerte für {}", item.path));
        }
        let changed = tx.execute(
            "INSERT INTO track_analysis(path,bpm,bpm_confidence,key,key_name,camelot,key_confidence,beat_offset_seconds) SELECT ?1,?2,?3,?4,?5,?6,?7,?8 WHERE EXISTS(SELECT 1 FROM tracks WHERE path=?1) ON CONFLICT(path) DO UPDATE SET bpm=excluded.bpm,bpm_confidence=excluded.bpm_confidence,key=excluded.key,key_name=excluded.key_name,camelot=excluded.camelot,key_confidence=excluded.key_confidence,beat_offset_seconds=excluded.beat_offset_seconds,analyzed_at=CURRENT_TIMESTAMP",
            params![item.path,item.bpm,item.confidence,item.key,item.key_name,item.camelot,item.key_confidence,item.beat_offset_seconds],
        ).map_err(|e| e.to_string())?;
        if changed == 0 {
            return Err(format!(
                "Analysierter Titel ist nicht mehr in der Bibliothek: {}",
                item.path
            ));
        }
    }
    tx.commit().map_err(|e| e.to_string())
}

#[tauri::command]
fn list_track_analyses(app: tauri::AppHandle) -> Result<Vec<TrackAnalysis>, String> {
    let db = database(&app)?;
    let mut stmt = db.prepare("SELECT path,bpm,bpm_confidence,key,key_name,camelot,key_confidence,beat_offset_seconds FROM track_analysis ORDER BY path").map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok(TrackAnalysis {
                path: row.get(0)?,
                bpm: row.get(1)?,
                confidence: row.get(2)?,
                key: row.get(3)?,
                key_name: row.get(4)?,
                camelot: row.get(5)?,
                key_confidence: row.get(6)?,
                beat_offset_seconds: row.get(7)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

fn detect_key(samples: &[f32], sample_rate: f64) -> Result<(String, String, String, f64), String> {
    const SIZE: usize = 8192;
    const HOP: usize = SIZE / 2;
    const MAJOR: [f64; 12] = [
        6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88,
    ];
    const MINOR: [f64; 12] = [
        6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17,
    ];
    const NOTES: [&str; 12] = [
        "C", "C#", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B",
    ];
    const MAJOR_CAMELOT: [&str; 12] = [
        "8B", "3B", "10B", "5B", "12B", "7B", "2B", "9B", "4B", "11B", "6B", "1B",
    ];
    const MINOR_CAMELOT: [&str; 12] = [
        "5A", "12A", "7A", "2A", "9A", "4A", "11A", "6A", "1A", "8A", "3A", "10A",
    ];
    if samples.len() < SIZE {
        return Err("Zu wenig Audiodaten für die Tonart-Analyse".into());
    }
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(SIZE);
    let mut profile = [0.0f64; 12];
    let mut frames = 0usize;
    let mut buffer = vec![Complex::new(0.0f32, 0.0f32); SIZE];
    let mut start = 0usize;
    while start + SIZE <= samples.len() {
        for i in 0..SIZE {
            let hann = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / (SIZE - 1) as f32).cos();
            buffer[i] = Complex::new(samples[start + i] * hann, 0.0);
        }
        fft.process(&mut buffer);
        for (bin, value) in buffer.iter().enumerate().take(SIZE / 2).skip(1) {
            let frequency = bin as f64 * sample_rate / SIZE as f64;
            if !(55.0..=1760.0).contains(&frequency) {
                continue;
            }
            let midi = (69.0 + 12.0 * (frequency / 440.0).log2()).round() as i32;
            let pitch_class = midi.rem_euclid(12) as usize;
            profile[pitch_class] += value.norm_sqr() as f64 / frequency.sqrt();
        }
        frames += 1;
        start += HOP;
    }
    if frames == 0 {
        return Err("Tonart konnte nicht aus der Audiodatei geschätzt werden".into());
    }
    let mean = profile.iter().sum::<f64>() / 12.0;
    let norm = profile
        .iter()
        .map(|x| (x - mean).powi(2))
        .sum::<f64>()
        .sqrt();
    if norm <= f64::EPSILON {
        return Err("Kein tonaler Inhalt für eine Tonart-Schätzung erkannt".into());
    }
    let normalized: Vec<f64> = profile.iter().map(|x| (x - mean) / norm).collect();
    let mut candidates = Vec::new();
    for (mode, template) in [("major", &MAJOR), ("minor", &MINOR)] {
        let template_mean = template.iter().sum::<f64>() / 12.0;
        let centered: Vec<f64> = template.iter().map(|x| x - template_mean).collect();
        let template_norm = centered.iter().map(|x| x * x).sum::<f64>().sqrt();
        for root in 0..12 {
            let score = (0..12)
                .map(|i| normalized[i] * centered[(i + 12 - root) % 12])
                .sum::<f64>()
                / template_norm;
            candidates.push((score, root, mode));
        }
    }
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0));
    let best = candidates[0];
    let confidence = ((best.0 - candidates[1].0) / best.0.abs().max(0.05)).clamp(0.0, 1.0);
    let (tag_key, key_name, camelot) = if best.2 == "major" {
        (
            NOTES[best.1].to_string(),
            format!("{} major", NOTES[best.1]),
            MAJOR_CAMELOT[best.1].to_string(),
        )
    } else {
        (
            format!("{}m", NOTES[best.1]),
            format!("{} minor", NOTES[best.1]),
            MINOR_CAMELOT[best.1].to_string(),
        )
    };
    Ok((tag_key, key_name, camelot, confidence))
}

fn analyze_bpm_file(path: &Path) -> Result<BpmAnalysis, String> {
    let file = File::open(path).map_err(|e| format!("Datei konnte nicht geöffnet werden: {e}"))?;
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let source = MediaSourceStream::new(Box::new(file), Default::default());
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("Audioformat wird nicht unterstützt: {e}"))?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| "Keine Audiospur gefunden".to_string())?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("Audio kann nicht decodiert werden: {e}"))?;
    let mut envelope = Vec::<f64>::new();
    let mut energy_sum = 0.0f64;
    let mut frame_count = 0usize;
    let mut target_frames = 0usize;
    let mut frames_per_bin = 0usize;
    let mut analysis_samples = Vec::<f32>::new();
    let mut resample_phase = 0u64;
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(_)) => break,
            Err(SymphoniaError::ResetRequired) => {
                return Err("Audio-Spur konnte nicht zurückgesetzt werden".into())
            }
            Err(_) => continue,
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(audio) => audio,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("Decodierung fehlgeschlagen: {e}")),
        };
        let spec = *decoded.spec();
        let channels = spec.channels.count().max(1);
        if frames_per_bin == 0 {
            frames_per_bin = (spec.rate as usize / 100).max(1);
        }
        let mut samples = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        samples.copy_interleaved_ref(decoded);
        for frame in samples.samples().chunks_exact(channels) {
            let mono = frame.iter().map(|sample| *sample as f64).sum::<f64>() / channels as f64;
            resample_phase += 22050;
            if resample_phase >= spec.rate as u64 {
                resample_phase %= spec.rate as u64;
                analysis_samples.push(mono as f32);
            }
            let mean_square =
                frame.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / channels as f64;
            energy_sum += mean_square;
            frame_count += 1;
            target_frames += 1;
            if target_frames >= frames_per_bin {
                envelope.push((energy_sum / frame_count as f64).sqrt());
                energy_sum = 0.0;
                frame_count = 0;
                target_frames = 0;
            }
        }
    }
    if envelope.len() < 500 {
        return Err("Für eine verlässliche BPM-Analyse ist der Titel zu kurz oder leer".into());
    }
    let onset: Vec<f64> = envelope
        .windows(2)
        .map(|w| (w[1] - w[0]).max(0.0))
        .collect();
    let mean = onset.iter().sum::<f64>() / onset.len() as f64;
    let onset: Vec<f64> = onset.iter().map(|v| v - mean).collect();
    let mut best = (0usize, f64::NEG_INFINITY);
    let mut scores = Vec::new();
    for lag in 27..=120.min(onset.len() / 2) {
        let score = onset
            .iter()
            .take(onset.len() - lag)
            .zip(onset.iter().skip(lag))
            .map(|(a, b)| a * b)
            .sum::<f64>()
            / (onset.len() - lag) as f64;
        scores.push((lag, score));
        if score > best.1 {
            best = (lag, score);
        }
    }
    if best.0 == 0 || best.1 <= 0.0 {
        return Err("Kein ausreichend klarer Beat für die BPM-Erkennung gefunden".into());
    }
    let next = scores
        .iter()
        .filter(|(lag, _)| lag.abs_diff(best.0) > 3)
        .map(|(_, score)| *score)
        .fold(f64::NEG_INFINITY, f64::max);
    let confidence = if best.1 > 0.0 {
        ((best.1 - next.max(0.0)) / best.1).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let mut phase_scores = vec![0.0f64; best.0];
    for (index, strength) in onset.iter().enumerate() {
        phase_scores[index % best.0] += strength.max(0.0);
    }
    let beat_phase = phase_scores
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(phase, _)| phase)
        .unwrap_or(0);
    let (key, key_name, camelot, key_confidence) = detect_key(&analysis_samples, 22050.0)?;
    Ok(BpmAnalysis {
        bpm: 6000.0 / best.0 as f64,
        confidence,
        beat_offset_seconds: beat_phase as f64 / 100.0,
        key,
        key_name,
        camelot,
        key_confidence,
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MediaToolStatus { id: String, installed: bool, version: String, latest: String, update_available: bool }

fn media_tools_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir=app.path().app_data_dir().map_err(|e|e.to_string())?.join("tools");
    std::fs::create_dir_all(&dir).map_err(|e|format!("Werkzeugordner konnte nicht angelegt werden: {e}"))?;
    Ok(dir)
}
fn executable_name(name:&str)->String { if cfg!(windows) { format!("{name}.exe") } else { name.to_owned() } }
fn resolve_tool(app:&tauri::AppHandle,name:&str)->Option<PathBuf> {
    let local=media_tools_dir(app).ok()?.join(executable_name(name));
    if local.is_file(){return Some(local)}
    if name=="ffmpeg" { if let Some(local)=std::env::var_os("LOCALAPPDATA") {let links=PathBuf::from(local).join("Microsoft/WinGet/Links/ffmpeg.exe");if links.is_file(){return Some(links)}} }
    let candidates=match name {
        "ffmpeg"=>vec!["/opt/homebrew/bin/ffmpeg","/usr/local/bin/ffmpeg","/usr/bin/ffmpeg","C:\\Program Files\\ffmpeg\\bin\\ffmpeg.exe"],
        "yt-dlp"=>vec!["/opt/homebrew/bin/yt-dlp","/usr/local/bin/yt-dlp","/usr/bin/yt-dlp"],
        "deno"=>vec!["/opt/homebrew/bin/deno","/usr/local/bin/deno","/usr/bin/deno"],
        _=>vec![],
    };
    candidates.iter().find(|p|Path::new(p).is_file()).map(PathBuf::from).or_else(||Some(PathBuf::from(executable_name(name))))
}
fn tool_version(path:&Path,name:&str)->String {
    let arg=if name=="ffmpeg" {"-version"} else {"--version"};
    Command::new(path).arg(arg).output().ok().filter(|o|o.status.success()).map(|o|String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").trim().to_owned()).filter(|s|!s.is_empty()).unwrap_or_default()
}
fn normalized_version(s:&str)->String {
    s.split_whitespace().map(|part|part.trim_start_matches('v')).find(|part|part.chars().next().map(|c|c.is_ascii_digit()).unwrap_or(false)).unwrap_or("")
        .chars().take_while(|c|c.is_ascii_digit()||*c=='.').collect()
}
async fn latest_tag(repo:&str)->Result<String,String>{
    let json=reqwest::Client::new().get(format!("https://api.github.com/repos/{repo}/releases/latest"))
      .header(reqwest::header::USER_AGENT,"MetaMagic desktop tool manager")
      .send().await.map_err(|e|format!("Release-Informationen für {repo} konnten nicht geladen werden: {e}"))?
      .error_for_status().map_err(|e|format!("Release-API {repo}: {e}"))?.json::<serde_json::Value>().await.map_err(|e|e.to_string())?;
    json["tag_name"].as_str().map(str::to_owned).ok_or_else(||format!("Keine aktuelle Version für {repo} gefunden"))
}
async fn install_github_asset(repo:&str,asset_name:&str,target:&Path,zip_member:Option<&str>)->Result<(),String>{
    let client=reqwest::Client::new();
    let release=client.get(format!("https://api.github.com/repos/{repo}/releases/latest"))
        .header(reqwest::header::USER_AGENT,"MetaMagic desktop tool manager").send().await
        .map_err(|e|e.to_string())?.error_for_status().map_err(|e|e.to_string())?
        .json::<serde_json::Value>().await.map_err(|e|e.to_string())?;
    let asset=release["assets"].as_array().and_then(|assets|assets.iter().find(|a|a["name"].as_str()==Some(asset_name)))
        .ok_or_else(||format!("Asset {asset_name} fehlt im Release von {repo}"))?;
    let url=asset["browser_download_url"].as_str().ok_or("Release enthält keine Download-Adresse")?;
    let bytes=client.get(url).header(reqwest::header::USER_AGENT,"MetaMagic desktop tool manager")
        .send().await.map_err(|e|format!("Download fehlgeschlagen: {e}"))?.error_for_status().map_err(|e|e.to_string())?
        .bytes().await.map_err(|e|format!("Download konnte nicht gelesen werden: {e}"))?;
    if let Some(expected)=asset["digest"].as_str().and_then(|d|d.strip_prefix("sha256:")) {
        let actual=format!("{:x}",Sha256::digest(&bytes));
        if !actual.eq_ignore_ascii_case(expected){return Err(format!("SHA-256-Prüfung für {asset_name} ist fehlgeschlagen"));}
    }
    let output=if let Some(member)=zip_member {
        let mut archive=ZipArchive::new(Cursor::new(bytes)).map_err(|e|format!("{asset_name} ist kein gültiges ZIP: {e}"))?;
        let mut file=archive.by_name(member).map_err(|e|format!("{member} fehlt in {asset_name}: {e}"))?;
        let mut extracted=Vec::new();file.read_to_end(&mut extracted).map_err(|e|e.to_string())?;extracted
    } else {bytes.to_vec()};
    let temp=target.with_extension(format!("download-{}",std::process::id()));
    {let mut file=File::create(&temp).map_err(|e|e.to_string())?;file.write_all(&output).map_err(|e|e.to_string())?;file.sync_all().map_err(|e|e.to_string())?;}
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt;std::fs::set_permissions(&temp,std::fs::Permissions::from_mode(0o755)).map_err(|e|e.to_string())?; }
    let backup=target.with_extension(format!("backup-{}",std::process::id()));
    let had_target=target.exists();
    if had_target {let _=std::fs::remove_file(&backup);std::fs::rename(target,&backup).map_err(|e|format!("Alte Werkzeugversion konnte nicht gesichert werden: {e}"))?;}
    if let Err(error)=std::fs::rename(&temp,target){if had_target{let _=std::fs::rename(&backup,target);}let _=std::fs::remove_file(&temp);return Err(format!("Werkzeug konnte nicht installiert werden: {error}"))}
    if had_target{let _=std::fs::remove_file(backup);}
    Ok(())
}
fn ytdlp_asset()->Result<&'static str,String>{
    #[cfg(target_os="macos")] {return Ok("yt-dlp_macos")}
    #[cfg(target_os="windows")] {if cfg!(target_arch="aarch64"){return Ok("yt-dlp_arm64.exe")}return Ok("yt-dlp.exe")}
    #[cfg(target_os="linux")] {if cfg!(target_arch="aarch64"){return Ok("yt-dlp_linux_aarch64")}if cfg!(target_arch="x86_64"){return Ok("yt-dlp_linux")}return Err("Diese Linux-Architektur wird von den offiziellen yt-dlp-Builds nicht unterstützt".into())}
    #[allow(unreachable_code)] Err("Betriebssystem wird nicht unterstützt".into())
}
fn deno_asset()->Result<&'static str,String>{
    #[cfg(target_os="macos")] {if cfg!(target_arch="aarch64"){return Ok("deno-aarch64-apple-darwin.zip")}return Ok("deno-x86_64-apple-darwin.zip")}
    #[cfg(target_os="windows")] {if cfg!(target_arch="aarch64"){return Ok("deno-aarch64-pc-windows-msvc.zip")}return Ok("deno-x86_64-pc-windows-msvc.zip")}
    #[cfg(target_os="linux")] {if cfg!(target_arch="aarch64"){return Ok("deno-aarch64-unknown-linux-gnu.zip")}return Ok("deno-x86_64-unknown-linux-gnu.zip")}
    #[allow(unreachable_code)] Err("Betriebssystem wird nicht unterstützt".into())
}
fn install_system_ffmpeg(app:&tauri::AppHandle)->Result<(),String>{
    #[cfg(target_os="macos")] {
        let brew=["/opt/homebrew/bin/brew","/usr/local/bin/brew"].iter().find(|p|Path::new(p).is_file());if brew.is_none()&&resolve_tool(app,"ffmpeg").map(|p|!tool_version(&p,"ffmpeg").is_empty()).unwrap_or(false){return Ok(())}let brew=brew.ok_or("Homebrew fehlt; installiere Homebrew oder FFmpeg und versuche es erneut")?;
        let installed=Command::new(brew).args(["list","--versions","ffmpeg"]).output().ok().map(|o|o.status.success()&&!o.stdout.is_empty()).unwrap_or(false);
        let action=if installed{"upgrade"}else{"install"};
        let result=Command::new(brew).args([action,"ffmpeg"]).output().map_err(|e|e.to_string())?;
        if !result.status.success(){return Err(format!("Homebrew konnte FFmpeg nicht installieren/aktualisieren: {}",String::from_utf8_lossy(&result.stderr)));}
        return Ok(())
    }
    #[cfg(target_os="windows")] {
        let installed=Command::new("winget").args(["list","--exact","--id","Gyan.FFmpeg.Shared","--disable-interactivity"]).output().ok().map(|o|o.status.success()&&!String::from_utf8_lossy(&o.stdout).contains("No installed package found")).unwrap_or(false);if !installed&&resolve_tool(app,"ffmpeg").map(|p|!tool_version(&p,"ffmpeg").is_empty()).unwrap_or(false){return Ok(())}
        let action=if installed{"upgrade"}else{"install"};
        let result=Command::new("winget").args([action,"--exact","--id","Gyan.FFmpeg.Shared","--silent","--accept-package-agreements","--accept-source-agreements"]).output().map_err(|e|format!("winget konnte nicht gestartet werden: {e}"))?;
        if !result.status.success(){return Err(format!("winget konnte FFmpeg nicht installieren/aktualisieren: {}",String::from_utf8_lossy(&result.stderr)));}
        return Ok(())
    }
    #[cfg(target_os="linux")] {
        let (program,args)=if Path::new("/usr/bin/apt-get").exists(){("pkexec",vec!["apt-get","install","-y","ffmpeg"])}else if Path::new("/usr/bin/dnf").exists(){("pkexec",vec!["dnf","install","-y","ffmpeg"])}else if Path::new("/usr/bin/pacman").exists(){("pkexec",vec!["pacman","-S","--noconfirm","ffmpeg"])}else{return Err("Kein unterstützter Linux-Paketmanager (apt, dnf oder pacman) gefunden".into())};
        let result=Command::new(program).args(args).output().map_err(|e|format!("FFmpeg-Installation konnte nicht gestartet werden: {e}"))?;
        if !result.status.success(){return Err(format!("Linux-Paketmanager konnte FFmpeg nicht installieren: {}",String::from_utf8_lossy(&result.stderr)));}
        return Ok(())
    }
    #[allow(unreachable_code)] Err("Betriebssystem wird nicht unterstützt".into())
}

fn ffmpeg_update_available()->bool{
    #[cfg(target_os="macos")] {let brew=["/opt/homebrew/bin/brew","/usr/local/bin/brew"].iter().find(|p|Path::new(p).is_file());return brew.and_then(|b|Command::new(b).args(["outdated","--quiet","ffmpeg"]).output().ok()).map(|o|o.status.success()&&!o.stdout.is_empty()).unwrap_or(false)}
    #[cfg(target_os="windows")] {return Command::new("winget").args(["upgrade","--disable-interactivity","--accept-source-agreements"]).output().ok().map(|o|{let text=String::from_utf8_lossy(&o.stdout).to_lowercase();o.status.success()&&text.lines().any(|line|line.contains("gyan.ffmpeg.shared"))}).unwrap_or(false)}
    #[cfg(target_os="linux")] {if Path::new("/usr/bin/apt-cache").exists(){return Command::new("apt-cache").args(["policy","ffmpeg"]).output().ok().map(|o|{let text=String::from_utf8_lossy(&o.stdout);let installed=text.lines().find(|l|l.trim_start().starts_with("Installed:")).and_then(|l|l.split(':').nth(1)).unwrap_or("").trim();let candidate=text.lines().find(|l|l.trim_start().starts_with("Candidate:")).and_then(|l|l.split(':').nth(1)).unwrap_or("").trim();candidate!="(none)"&&candidate!=installed}).unwrap_or(false)}return false}
    #[allow(unreachable_code)] false
}

#[tauri::command]
async fn check_media_tools(app:tauri::AppHandle)->Result<Vec<MediaToolStatus>,String>{
    let yt_latest=latest_tag("yt-dlp/yt-dlp").await.unwrap_or_default();
    let deno_latest=latest_tag("denoland/deno").await.unwrap_or_default();
    let yt_path=resolve_tool(&app,"yt-dlp");let deno_path=resolve_tool(&app,"deno");
    let mut statuses=Vec::new();
    for (id,name,path,latest) in [("yt-dlp","yt-dlp",yt_path,yt_latest),("ffmpeg","ffmpeg",resolve_tool(&app,"ffmpeg"),String::new()),("deno","deno",deno_path,deno_latest)] {
        let version=path.as_ref().map(|p|tool_version(p,name)).unwrap_or_default();let installed=!version.is_empty();
        let update_available=if id=="ffmpeg"{installed&&ffmpeg_update_available()}else{installed&&!latest.is_empty()&&normalized_version(&version)!=normalized_version(&latest)};
        statuses.push(MediaToolStatus{id:id.into(),installed,version,latest,update_available});
    }
    Ok(statuses)
}

#[tauri::command]
async fn install_or_update_media_tools(app:tauri::AppHandle)->Result<(),String>{
    let dir=media_tools_dir(&app)?;
    let ytdlp=dir.join(executable_name("yt-dlp"));let deno=dir.join(executable_name("deno"));
    let yt_asset=ytdlp_asset()?;let d_asset=deno_asset()?;
    install_github_asset("yt-dlp/yt-dlp",yt_asset,&ytdlp,None).await?;
    install_github_asset("denoland/deno",d_asset,&deno,Some(if cfg!(windows){"deno.exe"}else{"deno"})).await?;
    install_system_ffmpeg(&app)?;
    let yt_version=tool_version(&ytdlp,"yt-dlp");if yt_version.is_empty(){return Err("Installiertes yt-dlp ließ sich nicht starten".into())}
    let deno_version=tool_version(&deno,"deno");if deno_version.is_empty(){return Err("Installiertes Deno ließ sich nicht starten".into())}
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YoutubeMusicHit { video_id: String, title: String, artist: String, album: String, duration_seconds: u64, cover_url: String }

#[tauri::command]
async fn search_youtube_music(query: String) -> Result<Vec<YoutubeMusicHit>, String> {
    let query = query.trim();
    if query.is_empty() { return Err("Bitte einen Titel oder Interpreten eingeben".into()); }
    let payload = serde_json::json!({
        "context":{"client":{"clientName":"WEB_REMIX","clientVersion":"1.20251020.01.00","hl":"de","gl":"DE"}},
        "query":query,"params":"EgWKAQIIAWoMEA4QChADEAQQCRAF"
    });
    let response = reqwest::Client::new().post("https://music.youtube.com/youtubei/v1/search?prettyPrint=false")
        .header(reqwest::header::USER_AGENT,"Mozilla/5.0")
        .header(reqwest::header::ORIGIN,"https://music.youtube.com")
        .json(&payload).send().await.map_err(|e|format!("YouTube Music konnte nicht erreicht werden: {e}"))?;
    if !response.status().is_success() { return Err(format!("YouTube Music antwortete mit HTTP {}",response.status())); }
    let root: serde_json::Value=response.json().await.map_err(|e|format!("Suchantwort konnte nicht gelesen werden: {e}"))?;
    let sections=root["contents"]["tabbedSearchResultsRenderer"]["tabs"][0]["tabRenderer"]["content"]["sectionListRenderer"]["contents"].as_array().cloned().unwrap_or_default();
    let mut hits=Vec::new();
    for section in sections {
        let items=section["musicShelfRenderer"]["contents"].as_array().cloned().unwrap_or_default();
        for item in items {
            let renderer=&item["musicResponsiveListItemRenderer"];
            let title_run=renderer["flexColumns"][0]["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"][0].clone();
            let title=title_run["text"].as_str().unwrap_or("").to_owned();
            let video_id=renderer["playlistItemData"]["videoId"].as_str()
                .or(title_run["navigationEndpoint"]["watchEndpoint"]["videoId"].as_str()).unwrap_or("").to_owned();
            if title.is_empty() || video_id.is_empty() { continue; }
            let runs=renderer["flexColumns"][1]["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"].as_array().cloned().unwrap_or_default();
            let mut segments=vec![String::new()];let mut album=String::new();
            for run in runs {
                let text=run["text"].as_str().unwrap_or("");
                if text==" • " { segments.push(String::new()); continue; }
                let page=run["navigationEndpoint"]["browseEndpoint"]["browseEndpointContextSupportedConfigs"]["browseEndpointContextMusicConfig"]["pageType"].as_str().unwrap_or("");
                if page=="MUSIC_PAGE_TYPE_ALBUM" { album=text.to_owned(); }
                if let Some(last)=segments.last_mut(){last.push_str(text);}
            }
            let duration_text=segments.iter().rev().find(|part|part.split(':').all(|v|v.chars().all(|c|c.is_ascii_digit())) && part.contains(':')).cloned().unwrap_or_default();
            let duration_seconds=duration_text.split(':').filter_map(|v|v.parse::<u64>().ok()).fold(0,|acc,v|acc*60+v);
            let artist=segments.first().cloned().unwrap_or_default().replace(" und ",", ").replace(" & ",", ");
            let thumbs=renderer["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"].as_array();
            let cover_url=thumbs.and_then(|a|a.last()).and_then(|v|v["url"].as_str()).unwrap_or("").to_owned();
            hits.push(YoutubeMusicHit{video_id,title,artist,album,duration_seconds,cover_url});
            if hits.len()>=20 { return Ok(hits); }
        }
    }
    Ok(hits)
}

#[derive(Debug,Clone,Serialize)]
#[serde(rename_all="camelCase")]
struct YoutubeDownloadResult { title:String, path:Option<String>, error:Option<String> }
#[derive(Debug,Clone,Serialize)]
#[serde(rename_all="camelCase")]
struct YoutubeDownloadProgress { current:usize,total:usize,title:String,percent:u32 }
fn safe_media_name(value:&str)->String {
    let cleaned=value.chars().map(|c|if c.is_control()||matches!(c,'/'|'\\'|':'|'*'|'?'|'"'|'<'|'>'|'|'){ '_' }else{c}).collect::<String>();
    let result=cleaned.trim().trim_matches('.').to_owned(); if result.is_empty(){"Untitled".into()}else{result.chars().take(160).collect()}
}
fn write_youtube_music_tags(path:&Path,title:&str,artist:&str,album:&str)->Result<(),String>{
    let mut tagged=read_tagged(path)?;
    let tag_type=tagged.primary_tag_type();
    if tagged.tag(tag_type).is_none(){tagged.insert_tag(Tag::new(tag_type));}
    let tag=tagged.tag_mut(tag_type).ok_or("MP3 enthält keinen beschreibbaren Tag")?;
    tag.set_title(title.to_owned());
    if artist.trim().is_empty(){tag.remove_artist()}else{tag.set_artist(artist.to_owned())}
    if album.trim().is_empty(){tag.remove_album()}else{tag.set_album(album.to_owned())}
    tagged.save_to_path(path,write_options_for(path)).map_err(|e|format!("MP3-Tags konnten nicht gespeichert werden: {e}"))
}

#[tauri::command]
async fn download_youtube_music(app:tauri::AppHandle,items:Vec<YoutubeMusicHit>,output_dir:String,bitrate_kbps:u32,on_progress:Channel<YoutubeDownloadProgress>)->Result<Vec<YoutubeDownloadResult>,String>{
    if items.is_empty(){return Err("Bitte mindestens einen Suchtreffer markieren".into())}
    if !matches!(bitrate_kbps,128|192|256|320){return Err("MP3-Bitrate muss 128, 192, 256 oder 320 kbit/s sein".into())}
    let dir=PathBuf::from(output_dir);if !dir.is_dir(){return Err("Der gewählte Speicherort ist kein vorhandener Ordner".into())}
    let yt=resolve_tool(&app,"yt-dlp").ok_or("yt-dlp ist nicht installiert")?;let ff=resolve_tool(&app,"ffmpeg").ok_or("FFmpeg ist nicht installiert")?;let deno=resolve_tool(&app,"deno").ok_or("Deno ist nicht installiert")?;
    if tool_version(&yt,"yt-dlp").is_empty(){return Err("yt-dlp konnte nicht gestartet werden".into())}if tool_version(&ff,"ffmpeg").is_empty(){return Err("FFmpeg konnte nicht gestartet werden".into())}if tool_version(&deno,"deno").is_empty(){return Err("Deno konnte nicht gestartet werden".into())}
    let total=items.len();
    tauri::async_runtime::spawn_blocking(move||{
        let mut results=Vec::new();
        for (index,item) in items.into_iter().enumerate() {
            let current=index+1;
            let _=on_progress.send(YoutubeDownloadProgress{current,total,title:item.title.clone(),percent:0});
            let stem=safe_media_name(&format!("{} - {}",if item.artist.is_empty(){"Unbekannter Interpret"}else{&item.artist},item.title));
            let mut target=dir.join(format!("{stem}.mp3"));let mut n=2;while target.exists(){target=dir.join(format!("{stem} ({n}).mp3"));n+=1}
            let output_template=target.with_extension("%(ext)s");
            let mut command=Command::new(&yt);command.args(["--no-playlist","--extract-audio","--audio-format","mp3","--audio-quality"])
                .arg(format!("{bitrate_kbps}K"))
                .args(["--embed-thumbnail","--convert-thumbnails","jpg","--embed-metadata","--no-overwrites","--newline","--progress-template","download:%(progress._percent_str)s","--remote-components","ejs:github","--js-runtimes"])
                .arg(format!("deno:{}",deno.display())).args(["--ffmpeg-location"]).arg(&ff);
            command.args(["-o"]).arg(output_template.to_string_lossy().replace('%',"%%").replace("%%(ext)s","%(ext)s"))
                .arg(format!("https://www.youtube.com/watch?v={}",item.video_id)).stdout(Stdio::null()).stderr(Stdio::piped());
            let run=(||->Result<(bool,String),String>{
                let mut child=command.spawn().map_err(|e|format!("yt-dlp konnte nicht gestartet werden: {e}"))?;
                let stderr=child.stderr.take().ok_or("yt-dlp-Ausgabe konnte nicht gelesen werden")?;
                let mut tail=std::collections::VecDeque::new();
                for line in BufReader::new(stderr).lines(){let line=line.map_err(|e|e.to_string())?;let trimmed=line.trim();
                    if let Some(percent)=trimmed.strip_prefix("download:").and_then(|p|p.trim().trim_end_matches('%').parse::<f32>().ok()) {let _=on_progress.send(YoutubeDownloadProgress{current,total,title:item.title.clone(),percent:percent.clamp(0.0,100.0) as u32});continue;}
                    if !trimmed.is_empty(){tail.push_back(trimmed.to_owned());if tail.len()>12{tail.pop_front();}}
                }
                let status=child.wait().map_err(|e|e.to_string())?;
                Ok((status.success(),tail.into_iter().rev().find(|l|!l.is_empty()).unwrap_or_else(||"yt-dlp konnte den Titel nicht laden".into())))
            })();
            match run {
                Ok((true,_)) if target.is_file()=>{
                    let tag_error=write_youtube_music_tags(&target,&item.title,&item.artist,&item.album).err();
                    let _=on_progress.send(YoutubeDownloadProgress{current,total,title:item.title.clone(),percent:100});
                    results.push(YoutubeDownloadResult{title:item.title,path:Some(target.to_string_lossy().into_owned()),error:tag_error});
                },
                Ok((_,error))=>results.push(YoutubeDownloadResult{title:item.title,path:None,error:Some(error)}),
                Err(error)=>results.push(YoutubeDownloadResult{title:item.title,path:None,error:Some(error)}),
            }
        } Ok(results)
    }).await.map_err(|e|e.to_string())?
}

#[tauri::command]
async fn convert_local_video_to_mp3(input_path: String, output_path: String, bitrate_kbps: u32) -> Result<(), String> {
    if !matches!(bitrate_kbps, 128 | 192 | 256 | 320) { return Err("MP3-Bitrate muss 128, 192, 256 oder 320 kbit/s sein".into()); }
    let input = PathBuf::from(&input_path);
    let output = PathBuf::from(&output_path);
    if !input.is_file() { return Err("Die ausgewählte Videodatei existiert nicht mehr".into()); }
    if input == output { return Err("Eingabe und Ausgabe dürfen nicht dieselbe Datei sein".into()); }
    if output.extension().and_then(|ext| ext.to_str()).map(|ext| !ext.eq_ignore_ascii_case("mp3")).unwrap_or(true) {
        return Err("Bitte einen Ausgabedateinamen mit .mp3 wählen".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let ffmpeg = ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"]
            .iter().find(|candidate| Path::new(candidate).is_file()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("ffmpeg"));
        let result = Command::new(ffmpeg).args(["-nostdin", "-hide_banner", "-loglevel", "error", "-n", "-i"])
            .arg(&input).args(["-map", "0:a:0", "-vn", "-codec:a", "libmp3lame", "-b:a"])
            .arg(format!("{bitrate_kbps}k")).arg(&output).output()
            .map_err(|e| format!("FFmpeg konnte nicht gestartet werden. Installiere FFmpeg und stelle sicher, dass es im PATH verfügbar ist: {e}"))?;
        if !result.status.success() {
            let message = String::from_utf8_lossy(&result.stderr).trim().to_owned();
            let _ = std::fs::remove_file(&output);
            return Err(if message.is_empty() { "FFmpeg konnte die Videodatei nicht umwandeln".into() } else { message });
        }
        Ok(())
    }).await.map_err(|e| format!("Konvertierungsprozess fehlgeschlagen: {e}"))?
}

#[tauri::command]
async fn analyze_bpm(path: String) -> Result<BpmAnalysis, String> {
    tauri::async_runtime::spawn_blocking(move || analyze_bpm_file(Path::new(&path)))
        .await
        .map_err(|e| format!("Analyse konnte nicht gestartet werden: {e}"))?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_persisted_scope::init())
        .invoke_handler(tauri::generate_handler![
            scan_files,
            search_musicbrainz,
            search_album_covers,
            apply_album_cover,
            search_lyrics,
            save_lyrics_sidecar,
            preview_rename,
            rename_from_tags,
            list_tracks,
            list_library_folders,
            browse_filesystem,
            list_playlists,
            list_favorites,
            set_favorite,
            remove_tracks_from_library,
            reveal_file,
            get_playlist_tracks,
            create_playlist,
            rename_playlist,
            delete_playlist,
            add_tracks_to_playlist,
            remove_track_from_playlist,
            remove_tracks_from_playlist,
            reorder_playlist_tracks,
            write_tags,
            undo_tag_changes,
            redo_tag_changes,
            list_tag_history,
            inspect_file,
            export_folder_artwork,
            get_track_artwork,
            get_track_pictures,
            add_track_picture,
            remove_track_picture,
            set_track_picture_type,
            find_duplicates,
            trash_duplicate_copies,
            analyze_bpm,
            convert_local_video_to_mp3,
            search_youtube_music,
            check_media_tools,
            install_or_update_media_tools,
            download_youtube_music,
            save_track_analyses,
            list_track_analyses
        ])
        .run(tauri::generate_context!())
        .expect("error while running MetaMagic");
}
