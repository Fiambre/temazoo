#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod decode;
mod login;
mod media;
mod update;
mod video;

use eframe::egui::{self, Color32};
use rodio::{DeviceSinkBuilder, Player, Source};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{mpsc, Arc, Mutex, RwLock},
    time::Duration,
};
use ytmusicapi::{BrowserAuth, Privacy, YTMusicClient};

#[derive(Clone, Default)]
struct Track {
    id: String,
    title: String,
    artist: String,
    secs: u32,
    thumb: String,
    cover: String,
    /// Added automatically by the "suggestions" switch.
    sug: bool,
    /// Playlist item id (needed to remove it from its playlist); empty when unknown.
    svid: String,
    /// Browse ids / name used to jump to the artist or album page; empty when unknown.
    artist_id: String,
    album_id: String,
    album: String,
}

/// First artist (`UC…`) and album (`MPREb…`) browse ids found anywhere inside `v`.
fn ids_in(v: &serde_json::Value) -> (String, String) {
    fn walk(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, x) in m {
                    match (k.as_str(), x.as_str()) {
                        ("browseId", Some(s)) => out.push(s.to_string()),
                        _ => walk(x, out),
                    }
                }
            }
            serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut all = Vec::new();
    walk(v, &mut all);
    let pick = |p: &str| all.iter().find(|s| s.starts_with(p)).cloned().unwrap_or_default();
    (pick("UC"), pick("MPREb"))
}

/// Audio format requested from YouTube. Persisted in `config.json`.
#[derive(Clone, Copy, PartialEq)]
enum Quality {
    Opus,
    Aac,
}

impl Quality {
    fn ext(self) -> &'static str {
        match self {
            Quality::Opus => "webm",
            Quality::Aac => "m4a",
        }
    }

    fn yt_format(self) -> &'static str {
        match self {
            Quality::Opus => "251/250/140",
            Quality::Aac => "140/bestaudio[ext=m4a]",
        }
    }

    fn load() -> Self {
        let aac = std::fs::read_to_string("config.json")
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .is_some_and(|v| v["audio_format"] == "aac");
        if aac {
            Quality::Aac
        } else {
            Quality::Opus
        }
    }

}

fn load_prefetch() -> bool {
    std::fs::read_to_string("config.json")
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .is_none_or(|v| v["prefetch"] != false)
}

fn load_autoplay() -> bool {
    std::fs::read_to_string("config.json")
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .is_some_and(|v| v["autoplay"] == true)
}

/// Show the video of songs that have one. Off by default: nothing video-related is loaded then.
fn load_video() -> bool {
    std::fs::read_to_string("config.json")
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .is_some_and(|v| v["video"] == true)
}

/// How the lyrics look over the fullscreen video.
#[derive(Clone, Copy, PartialEq)]
enum LyricsStyle {
    /// A few lines at the bottom of the picture, fading in and out as the song moves.
    Overlay,
    /// A side panel with the whole lyrics, picture on the left.
    Panel,
}

/// Lyrics settings: (style, lines visible in the overlay, show them when entering fullscreen).
fn load_lyrics_cfg() -> (LyricsStyle, u8, bool) {
    let v = std::fs::read_to_string("config.json").ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()).unwrap_or_default();
    let style = if v["lyrics_style"] == "panel" { LyricsStyle::Panel } else { LyricsStyle::Overlay };
    let lines = match v["lyrics_lines"].as_u64() {
        Some(1) => 1,
        Some(5) => 5,
        _ => 3,
    };
    (style, lines, v["lyrics_fullscreen"] == true)
}

/// Maximum video height in fullscreen: 480, 720 or 1080; 0 = whatever the screen needs (up to 1080).
fn load_fs_quality() -> u32 {
    std::fs::read_to_string("config.json")
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v["fullscreen_quality"].as_u64())
        .map_or(720, |n| match n {
            0 => 0,
            1..=480 => 480,
            481..=720 => 720,
            _ => 1080,
        })
}

fn save_config(q: Quality, prefetch: bool, autoplay: bool, video: bool, lyrics: (LyricsStyle, u8, bool), fs_quality: u32) {
    let f = if q == Quality::Aac { "aac" } else { "opus" };
    let style = if lyrics.0 == LyricsStyle::Panel { "panel" } else { "overlay" };
    let _ = std::fs::write(
        "config.json",
        format!(
            "{{\n  \"audio_format\": \"{f}\",\n  \"prefetch\": {prefetch},\n  \"autoplay\": {autoplay},\n  \"video\": {video},\n  \"lyrics_style\": \"{style}\",\n  \"lyrics_lines\": {},\n  \"lyrics_fullscreen\": {},\n  \"fullscreen_quality\": {}\n}}\n",
            lyrics.1, lyrics.2, fs_quality
        ),
    );
}

fn cache_size() -> u64 {
    std::fs::read_dir(cache_dir())
        .map(|rd| rd.flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum())
        .unwrap_or(0)
}

fn clear_cache() {
    if let Ok(rd) = std::fs::read_dir(cache_dir()) {
        for e in rd.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Cached file for `id` in exactly the format `q`.
fn cached(id: &str, q: Quality) -> Option<PathBuf> {
    Some(cache_dir().join(format!("{id}.{}", q.ext()))).filter(|p| p.exists())
}

/// Any file downloaded for `id`, preferring the extension of `q` (yt-dlp may fall back to AAC).
fn downloaded(id: &str, q: Quality) -> Option<PathBuf> {
    let other = if q == Quality::Opus { Quality::Aac } else { Quality::Opus };
    [q, other]
        .iter()
        .map(|q| cache_dir().join(format!("{id}.{}", q.ext())))
        .find(|p| p.exists())
}

/// What a YouTube Music search looks for.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Songs,
    Videos,
    Artists,
    Albums,
    Playlists,
}

impl Kind {
    /// Results that are songs/videos (played directly) rather than cards to open.
    fn is_tracks(self) -> bool {
        matches!(self, Kind::Songs | Kind::Videos)
    }

    fn params(self) -> &'static str {
        match self {
            Kind::Songs => "EgWKAQIIAWoMEA4QChADEAQQCRAF",
            Kind::Videos => "EgWKAQIQAWoMEA4QChADEAQQCRAF",
            // Community playlists; the featured ones are requested separately (see `Cmd::Search`).
            Kind::Playlists => "EgeKAQQoAEABagwQDhAKEAMQBBAJEAU%3D",
            Kind::Albums => "EgWKAQIYAWoMEA4QChADEAQQCRAF",
            Kind::Artists => "EgWKAQIgAWoMEA4QChADEAQQCRAF",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum CardKind {
    Artist,
    Album,
    Playlist,
}

/// A non-playable entry (artist, album, playlist) that opens another view.
#[derive(Clone)]
struct Card {
    kind: CardKind,
    id: String,
    title: String,
    subtitle: String,
    thumb: String,
    section: String,
}

enum CRow {
    Label(String),
    Card(usize),
}

#[derive(Clone)]
enum FeedItem {
    Card(Card),
    Song(Track),
    /// A genre/mood/section button that opens another feed page.
    Chip { title: String, browse: String, params: Option<String> },
}

#[derive(Clone, Copy, PartialEq)]
enum ShelfKind {
    Cards,
    /// Songs laid out as rows in columns (like "Quick picks").
    Rows,
    Chips,
}

#[derive(Clone)]
struct Shelf {
    title: String,
    kind: ShelfKind,
    items: Vec<FeedItem>,
}

/// A browse page made of shelves (Home, Explore, a mood/genre category...), loaded in pages.
#[derive(Default)]
struct Feed {
    title: String,
    browse: String,
    params: Option<String>,
    shelves: Vec<Shelf>,
    /// Continuation token for the next page of shelves.
    next: Option<String>,
    loading: bool,
    loaded: bool,
}

const HOME: usize = 0;
const EXPLORE: usize = 1;
/// Dynamic page opened from a chip (category, charts, new releases...).
const PAGE: usize = 2;

#[derive(Clone, Copy, PartialEq)]
enum Priv {
    Public,
    Unlisted,
    Private,
}

impl Priv {
    fn api(self) -> &'static str {
        match self {
            Priv::Public => "PUBLIC",
            Priv::Unlisted => "UNLISTED",
            Priv::Private => "PRIVATE",
        }
    }
}

enum DlgKind {
    New { with: Option<Track> },
    Edit { id: String },
    Delete { id: String },
}

/// Modal dialog for creating, editing or deleting a playlist.
struct Dialog {
    kind: DlgKind,
    name: String,
    desc: String,
    privacy: Priv,
}

/// A previous view, to support "Volver".
struct Snap {
    tracks: Vec<Track>,
    cards: Vec<Card>,
    view_title: Option<String>,
    is_search: bool,
    search_q: String,
    sel_playlist: Option<usize>,
    page: Option<usize>,
    more_songs: Option<MoreSongs>,
}

#[derive(Clone)]
struct Account {
    name: String,
    handle: String,
    photo: String,
}

/// First value stored under `key` anywhere in the JSON tree.
fn find_key<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    match v {
        serde_json::Value::Object(m) => m.get(key).or_else(|| m.values().find_map(|x| find_key(x, key))),
        serde_json::Value::Array(a) => a.iter().find_map(|x| find_key(x, key)),
        _ => None,
    }
}

/// "[01:23.45] text" lines → (milliseconds, text), sorted. Metadata tags ("[ar:...]") are skipped; a line
/// with several time tags (repeated chorus) becomes one entry per tag.
fn parse_lrc(s: &str) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for line in s.lines() {
        let mut rest = line.trim();
        let mut times = Vec::new();
        while let Some(r) = rest.strip_prefix('[') {
            let Some(end) = r.find(']') else { break };
            let tag = &r[..end];
            let ms = tag.split_once(':').and_then(|(m, sec)| {
                let (m, sec) = (m.parse::<u32>().ok()?, sec.parse::<f64>().ok()?);
                Some(m * 60_000 + (sec * 1000.0) as u32)
            });
            match ms {
                Some(ms) => times.push(ms),
                None => break, // "[ar:Artist]" and the like
            }
            rest = r[end + 1..].trim_start();
        }
        // Some files also carry per-word times ("<00:19.25>no"): keep only the words.
        let mut text = String::with_capacity(rest.len());
        let mut in_tag = false;
        for ch in rest.chars() {
            match ch {
                '<' => in_tag = true,
                '>' if in_tag => in_tag = false,
                _ if !in_tag => text.push(ch),
                _ => {}
            }
        }
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for t in times {
            out.push((t, text.clone()));
        }
    }
    out.sort_by_key(|(t, _)| *t);
    out
}

/// Title without the decoration of video uploads: "(Official Video)", "[HD]", "(Remastered 2009)"...
fn clean_title(t: &str) -> String {
    let junk = ["official", "video", "lyric", "audio", "visualizer", "remaster", "hd", "4k", "live"];
    let mut out = String::new();
    let mut depth = 0;
    let mut group = String::new();
    for ch in t.chars() {
        match ch {
            '(' | '[' => {
                depth += 1;
                group.clear();
            }
            ')' | ']' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    let g = group.to_lowercase();
                    if !junk.iter().any(|j| g.contains(j)) {
                        out.push('(');
                        out.push_str(&group);
                        out.push(')');
                    }
                }
            }
            _ if depth > 0 => group.push(ch),
            _ => out.push(ch),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Synced lyrics from LRCLIB (free, no key). Uses the `curl.exe` that ships with Windows, so no HTTP client
/// is linked in. Exact lookup first, then a search accepting only results whose length matches the song.
async fn lrclib_synced(title: &str, artist: &str, secs: u32) -> Option<Vec<(u32, String)>> {
    async fn get(path: &str, params: &[(&str, String)]) -> Option<serde_json::Value> {
        let mut c = tokio::process::Command::new("curl.exe");
        c.args(["-fsSL", "--max-time", "8", "-A", "Temazoo (https://github.com/Fiambre/temazoo)", "-G"])
            .arg(format!("https://lrclib.net/api/{path}"));
        for (k, v) in params {
            c.arg("--data-urlencode").arg(format!("{k}={v}"));
        }
        c.creation_flags(0x0800_0000);
        let out = c.output().await.ok().filter(|o| o.status.success())?;
        serde_json::from_slice(&out.stdout).ok()
    }
    let synced = |v: &serde_json::Value| v["syncedLyrics"].as_str().filter(|s| !s.trim().is_empty()).map(parse_lrc).filter(|l| !l.is_empty());
    let title = clean_title(title);
    if title.is_empty() {
        return None;
    }
    let mut exact = vec![("track_name", title.clone()), ("artist_name", artist.to_string())];
    if secs > 0 {
        exact.push(("duration", secs.to_string()));
    }
    if let Some(l) = get("get", &exact).await.as_ref().and_then(synced) {
        return Some(l);
    }
    // Search: "artist title" first, then the title alone (video uploads carry the channel as artist).
    for q in [format!("{artist} {title}"), title.clone()] {
        let Some(v) = get("search", &[("q", q)]).await else { continue };
        let hit = v.as_array().into_iter().flatten().find(|r| {
            let ok_len = secs == 0 || r["duration"].as_f64().is_some_and(|d| (d - secs as f64).abs() <= 3.0);
            ok_len && synced(r).is_some()
        });
        if let Some(l) = hit.and_then(synced) {
            return Some(l);
        }
    }
    None
}

/// Browse id of the "Lyrics" tab in a `next` response (it starts with "MPLY"), if the song has one.
fn lyrics_browse_id(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Object(m) => m
            .get("browseId")
            .and_then(|b| b.as_str())
            .filter(|b| b.starts_with("MPLY"))
            .map(String::from)
            .or_else(|| m.values().find_map(lyrics_browse_id)),
        serde_json::Value::Array(a) => a.iter().find_map(lyrics_browse_id),
        _ => None,
    }
}

fn parse_account(v: &serde_json::Value) -> Option<Account> {
    let text = |k: &str| find_key(v, k).and_then(|x| x["runs"][0]["text"].as_str()).unwrap_or("").to_string();
    let name = text("accountName");
    if name.is_empty() {
        return None;
    }
    let photo = find_key(v, "accountPhoto").and_then(|x| x["thumbnails"][0]["url"].as_str()).unwrap_or("").to_string();
    Some(Account { name, handle: text("channelHandle"), photo })
}

enum Cmd {
    /// Load (or continue) a shelf page: `idx` is the feed slot, `token` the continuation.
    Feed { idx: usize, browse: String, params: Option<String>, token: Option<String> },
    /// Rebuild the YouTube Music client from the current `headers.json`.
    Reauth,
    /// Who is logged in (name, handle, avatar); answered with `Evt::Account`.
    Account,
    /// Fetch the ids of the user's liked songs (to draw the heart correctly).
    LoadLikes,
    Rate { vid: String, like: bool },
    /// Which of these playlists belong to the user (only those can be edited).
    CheckOwned(Vec<String>),
    LoadEdit(String),
    Create { title: String, desc: String, privacy: Priv, with: Option<String> },
    Edit { id: String, title: String, desc: String, privacy: Priv },
    DeletePl(String),
    AddTo { id: String, name: String, vid: String },
    RemoveFrom { id: String, vid: String, set: String },
    Playlists,
    Search(String, Kind),
    Artist(String),
    /// Autocomplete suggestions for what is being typed.
    Suggest(String),
    /// (browse id, params) of an artist's full list of songs.
    MoreSongs(String, Option<String>),
    Album(String),
    Radio(String),
    /// Looks up whether the song has a video and, if so, downloads one no taller than the given height
    /// (the smallest that looks right in the panel); answered with `Evt::Video`.
    Video(String, u32),
    /// Higher-resolution video for fullscreen: (song id, wanted height, height already in use).
    /// Answered with `Evt::VideoHd`; nothing is downloaded when there is no better version.
    VideoHd(String, u32, u32),
    /// Lyrics of a song: synced ones from LRCLIB when it has them, else the plain text of YouTube Music.
    /// (song id, title, artist, seconds); answered with `Evt::Lyrics`.
    Lyrics(String, String, String, u32),
    Tracks(String),
    Fetch(Track, bool, Quality),
}

enum Evt {
    Playlists(Vec<(String, String)>),
    Tracks(Vec<Track>),
    Ready(String, PathBuf),
    Prefetched,
    Results(String, Kind, Vec<Track>, Vec<Card>),
    Artist(String, Vec<Track>, Vec<Card>, Option<MoreSongs>),
    /// (typed text, suggestions)
    Suggest(String, Vec<String>),
    Radio(Vec<Track>),
    /// Video file for the song, or `None` when it has no video (or it couldn't be fetched).
    Video(String, Option<PathBuf>),
    VideoHd(String, Option<PathBuf>),
    /// Song id and its lyrics, if any.
    Lyrics(String, Option<Lyrics>),
    /// Exit code of a login/refresh child process (see `login`).
    Session(i32),
    Feed { idx: usize, shelves: Vec<Shelf>, next: Option<String>, first: bool },
    Likes(Vec<String>),
    RateFailed(String, bool, String),
    /// A media key was pressed (or the OS overlay was used).
    Media(souvlaki::MediaControlEvent),
    /// (owned ids, all ids that were checked)
    Owned(Vec<String>, Vec<String>),
    EditInfo(String, String, String, Priv),
    Created(String),
    Done { msg: String, reload: bool },
    Removed(String),
    /// Result of an update check (`manual` = the user asked for it, so say "up to date").
    Update { found: Option<update::Release>, manual: bool },
    UpdateFailed { msg: String, manual: bool },
    /// `None` = no active session.
    Account(Option<Account>),
    /// The installer is downloaded and verified.
    UpdateReady(PathBuf),
    Error(String),
}

enum Audio {
    Load(PathBuf),
    Pause,
    Resume,
    Volume(f32),
    Seek(Duration),
    /// Fade the current song out and stop it (used as soon as another one is requested).
    Fade,
}

#[derive(Default)]
struct AudioState {
    pos: Duration,
    /// When `pos` was measured (the video clock needs it: the audio thread only samples it every so often).
    at: Option<std::time::Instant>,
    total: Duration,
    finished: bool,
    serial: u64,
}

fn cache_dir() -> PathBuf {
    let d = std::env::temp_dir().join("temazoo-cache");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn parse_search(v: &serde_json::Value) -> Vec<Track> {
    let sections = &v["contents"]["tabbedSearchResultsRenderer"]["tabs"][0]["tabRenderer"]["content"]["sectionListRenderer"]["contents"];
    let text = |c: &serde_json::Value| -> String {
        c["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"]
            .as_array()
            .map(|r| r.iter().filter_map(|x| x["text"].as_str()).collect::<String>())
            .unwrap_or_default()
            .replace('\u{a0}', " ")
    };
    let url = |t: Option<&serde_json::Value>| t.and_then(|t| t["url"].as_str()).unwrap_or("").to_string();
    let mut out = Vec::new();
    for sec in sections.as_array().into_iter().flatten() {
        for it in sec["musicShelfRenderer"]["contents"].as_array().into_iter().flatten() {
            let i = &it["musicResponsiveListItemRenderer"];
            let Some(id) = i["playlistItemData"]["videoId"].as_str() else { continue };
            let cols = i["flexColumns"].as_array().cloned().unwrap_or_default();
            let meta = cols.get(1).map(text).unwrap_or_default();
            // "Artist • Album • 2:20"
            let parts: Vec<&str> = meta.split(" • ").collect();
            let thumbs = i["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"].as_array().cloned().unwrap_or_default();
            let (artist_id, album_id) = ids_in(&i["flexColumns"][1]);
            out.push(Track {
                id: id.to_string(),
                title: cols.first().map(text).unwrap_or_default(),
                artist: parts.first().copied().unwrap_or_default().to_string(),
                secs: parts.last().and_then(|p| parse_dur(p)).unwrap_or(0),
                thumb: resize(&url(thumbs.first()), 96),
                cover: resize(&url(thumbs.last()), 256),
                album: if album_id.is_empty() { String::new() } else { parts.get(1).copied().unwrap_or_default().to_string() },
                artist_id,
                album_id,
                ..Default::default()
            });
        }
    }
    out
}

fn runs(v: &serde_json::Value) -> String {
    v["runs"]
        .as_array()
        .map(|r| r.iter().filter_map(|x| x["text"].as_str()).collect::<String>())
        .unwrap_or_default()
        .replace('\u{a0}', " ")
}

fn list_col(i: &serde_json::Value, n: usize) -> String {
    runs(&i["flexColumns"][n]["musicResponsiveListItemFlexColumnRenderer"]["text"])
}

fn thumbs_of(v: &serde_json::Value) -> (String, String) {
    let t = v.as_array().cloned().unwrap_or_default();
    let url = |t: Option<&serde_json::Value>| t.and_then(|t| t["url"].as_str()).unwrap_or("").to_string();
    (resize(&url(t.first()), 96), resize(&url(t.last()), 256))
}

/// Artist / album results of a filtered search.
fn parse_search_cards(v: &serde_json::Value, kind: Kind) -> Vec<Card> {
    let sections = &v["contents"]["tabbedSearchResultsRenderer"]["tabs"][0]["tabRenderer"]["content"]["sectionListRenderer"]["contents"];
    let mut out = Vec::new();
    for sec in sections.as_array().into_iter().flatten() {
        for it in sec["musicShelfRenderer"]["contents"].as_array().into_iter().flatten() {
            let i = &it["musicResponsiveListItemRenderer"];
            let Some(id) = i["navigationEndpoint"]["browseEndpoint"]["browseId"].as_str() else { continue };
            out.push(Card {
                kind: match kind {
                    Kind::Artists => CardKind::Artist,
                    Kind::Playlists => CardKind::Playlist,
                    _ => CardKind::Album,
                },
                // Playlist browse ids carry a "VL" prefix that the playlist endpoints don't want.
                id: if kind == Kind::Playlists { id.strip_prefix("VL").unwrap_or(id).to_string() } else { id.to_string() },
                title: list_col(i, 0),
                subtitle: list_col(i, 1),
                thumb: thumbs_of(&i["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]).0,
                section: String::new(),
            });
        }
    }
    out
}

/// Where the full list of an artist's songs lives: (browse id, params).
type MoreSongs = (String, Option<String>);

/// "Songs" of an artist/album/playlist page that opens as its own list (`musicPlaylistShelfRenderer`).
fn parse_shelf_songs(v: &serde_json::Value) -> Vec<Track> {
    let Some(shelf) = find_key(v, "musicPlaylistShelfRenderer") else { return vec![] };
    let mut out = Vec::new();
    for it in shelf["contents"].as_array().into_iter().flatten() {
        let i = &it["musicResponsiveListItemRenderer"];
        let Some(id) = i["playlistItemData"]["videoId"].as_str() else { continue };
        let (thumb, cover) = thumbs_of(&i["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]);
        let (artist_id, _) = ids_in(&i["flexColumns"][1]);
        let (_, album_id) = ids_in(&i["flexColumns"][2]);
        out.push(Track {
            id: id.to_string(),
            title: list_col(i, 0),
            artist: list_col(i, 1),
            album: if album_id.is_empty() { String::new() } else { list_col(i, 2) },
            secs: parse_dur(&runs(&i["fixedColumns"][0]["musicResponsiveListItemFixedColumnRenderer"]["text"])).unwrap_or(0),
            thumb,
            cover,
            artist_id,
            album_id,
            ..Default::default()
        });
    }
    out
}

/// Artist page: top songs plus carousels of albums, singles, playlists and related artists.
fn parse_artist(v: &serde_json::Value) -> (String, Vec<Track>, Vec<Card>, Option<MoreSongs>) {
    let name = runs(&v["header"]["musicImmersiveHeaderRenderer"]["title"]);
    let secs = &v["contents"]["singleColumnBrowseResultsRenderer"]["tabs"][0]["tabRenderer"]["content"]["sectionListRenderer"]["contents"];
    let (mut tracks, mut cards, mut more) = (Vec::new(), Vec::new(), None);
    for s in secs.as_array().into_iter().flatten() {
        let shelf = &s["musicShelfRenderer"];
        if let Some(e) = shelf["bottomEndpoint"]["browseEndpoint"].as_object().filter(|_| shelf["contents"].is_array()) {
            more = e.get("browseId").and_then(|b| b.as_str()).map(|b| (b.to_string(), e.get("params").and_then(|p| p.as_str()).map(String::from)));
        }
        for it in shelf["contents"].as_array().into_iter().flatten() {
            let i = &it["musicResponsiveListItemRenderer"];
            let Some(id) = i["playlistItemData"]["videoId"].as_str() else { continue };
            let (thumb, cover) = thumbs_of(&i["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]);
            let (artist_id, _) = ids_in(&i["flexColumns"][1]);
            tracks.push(Track { id: id.to_string(), title: list_col(i, 0), artist: list_col(i, 1), thumb, cover, artist_id, ..Default::default() });
        }
        let car = &s["musicCarouselShelfRenderer"];
        let section = runs(&car["header"]["musicCarouselShelfBasicHeaderRenderer"]["title"]);
        for it in car["contents"].as_array().into_iter().flatten() {
            cards.extend(two_row_card(&it["musicTwoRowItemRenderer"], &section));
        }
    }
    (name, tracks, cards, more)
}

fn two_row_card(i: &serde_json::Value, section: &str) -> Option<Card> {
    let browse = i["navigationEndpoint"]["browseEndpoint"]["browseId"].as_str().unwrap_or("");
    let play = i["thumbnailOverlay"]["musicItemThumbnailOverlayRenderer"]["content"]["musicPlayButtonRenderer"]["playNavigationEndpoint"]["watchPlaylistEndpoint"]["playlistId"]
        .as_str()
        .unwrap_or("");
    let (kind, id) = if browse.starts_with("MPREb") {
        (CardKind::Album, browse)
    } else if browse.starts_with("UC") {
        (CardKind::Artist, browse)
    } else if browse.starts_with("VL") && !play.is_empty() {
        (CardKind::Playlist, play)
    } else {
        return None;
    };
    Some(Card {
        kind,
        id: id.to_string(),
        title: runs(&i["title"]),
        subtitle: runs(&i["subtitle"]),
        thumb: thumbs_of(&i["thumbnailRenderer"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]).0,
        section: section.to_string(),
    })
}

/// Carousels of an artist page that have a "see all" button: (section title, browse id, params).
fn carousel_more(v: &serde_json::Value) -> Vec<(String, String, Option<String>)> {
    let secs = &v["contents"]["singleColumnBrowseResultsRenderer"]["tabs"][0]["tabRenderer"]["content"]["sectionListRenderer"]["contents"];
    let mut out = Vec::new();
    for s in secs.as_array().into_iter().flatten() {
        let h = &s["musicCarouselShelfRenderer"]["header"]["musicCarouselShelfBasicHeaderRenderer"];
        let e = &h["moreContentButton"]["buttonRenderer"]["navigationEndpoint"]["browseEndpoint"];
        if let Some(b) = e["browseId"].as_str() {
            out.push((runs(&h["title"]), b.to_string(), e["params"].as_str().map(String::from)));
        }
    }
    out
}

/// All album-like items (grid or carousel) of a "see all" page.
fn parse_full_cards(v: &serde_json::Value, section: &str) -> Vec<Card> {
    fn walk(v: &serde_json::Value, section: &str, out: &mut Vec<Card>) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, x) in m {
                    if k == "musicTwoRowItemRenderer" {
                        out.extend(two_row_card(x, section));
                    } else {
                        walk(x, section, out);
                    }
                }
            }
            serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, section, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, section, &mut out);
    out
}

/// Artist name from a "Artist • 3.5M views" style subtitle (skipping a leading type word).
fn first_artist(sub: &str) -> String {
    let mut parts = sub.split(" • ");
    let first = parts.next().unwrap_or_default();
    if matches!(first, "Song" | "Video" | "Canción" | "Single" | "Album") {
        parts.next().unwrap_or_default().to_string()
    } else {
        first.to_string()
    }
}

fn feed_item(it: &serde_json::Value) -> Option<FeedItem> {
    if let Some(x) = it.get("musicTwoRowItemRenderer") {
        let (_, cover) = thumbs_of(&x["thumbnailRenderer"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]);
        let (title, subtitle) = (runs(&x["title"]), runs(&x["subtitle"]));
        if let Some(v) = x["navigationEndpoint"]["watchEndpoint"]["videoId"].as_str() {
            let (artist_id, album_id) = ids_in(&x["subtitle"]);
            return Some(FeedItem::Song(Track {
                id: v.to_string(),
                title,
                artist: first_artist(&subtitle),
                thumb: cover.clone(),
                cover,
                artist_id,
                album_id,
                ..Default::default()
            }));
        }
        let browse = x["navigationEndpoint"]["browseEndpoint"]["browseId"].as_str()?;
        let (kind, id) = if browse.starts_with("MPREb") {
            (CardKind::Album, browse.to_string())
        } else if browse.starts_with("UC") {
            (CardKind::Artist, browse.to_string())
        } else if let Some(pl) = browse.strip_prefix("VL") {
            (CardKind::Playlist, pl.to_string())
        } else {
            return None;
        };
        return Some(FeedItem::Card(Card { kind, id, title, subtitle, thumb: cover, section: String::new() }));
    }
    if let Some(x) = it.get("musicResponsiveListItemRenderer") {
        let id = x["playlistItemData"]["videoId"].as_str()?;
        let (thumb, cover) = thumbs_of(&x["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]);
        let (artist_id, album_id) = ids_in(&x["flexColumns"][1]);
        return Some(FeedItem::Song(Track {
            id: id.to_string(),
            title: list_col(x, 0),
            artist: first_artist(&list_col(x, 1)),
            thumb,
            cover,
            artist_id,
            album_id,
            ..Default::default()
        }));
    }
    if let Some(x) = it.get("musicNavigationButtonRenderer") {
        let e = &x["clickCommand"]["browseEndpoint"];
        return Some(FeedItem::Chip {
            title: runs(&x["buttonText"]),
            browse: e["browseId"].as_str()?.to_string(),
            params: e["params"].as_str().map(String::from),
        });
    }
    None
}

/// Shelves of a Home/Explore/category page plus the continuation token, if any.
fn parse_feed(sl: &serde_json::Value) -> (Vec<Shelf>, Option<String>) {
    let next = sl["continuations"][0]["nextContinuationData"]["continuation"].as_str().map(String::from);
    let mut out = Vec::new();
    for s in sl["contents"].as_array().into_iter().flatten() {
        let (title, raw): (String, Vec<serde_json::Value>) = if let Some(m) = s.get("musicCarouselShelfRenderer") {
            (
                runs(&m["header"]["musicCarouselShelfBasicHeaderRenderer"]["title"]),
                m["contents"].as_array().cloned().unwrap_or_default(),
            )
        } else if let Some(g) = s.get("gridRenderer") {
            (runs(&g["header"]["gridHeaderRenderer"]["title"]), g["items"].as_array().cloned().unwrap_or_default())
        } else if let Some(m) = s.get("musicShelfRenderer") {
            (runs(&m["title"]), m["contents"].as_array().cloned().unwrap_or_default())
        } else {
            continue;
        };
        let items: Vec<FeedItem> = raw.iter().filter_map(feed_item).collect();
        if items.is_empty() {
            continue;
        }
        let kind = if items.iter().all(|i| matches!(i, FeedItem::Chip { .. })) {
            ShelfKind::Chips
        } else if raw.iter().all(|r| r.get("musicResponsiveListItemRenderer").is_some()) {
            ShelfKind::Rows
        } else {
            ShelfKind::Cards
        };
        out.push(Shelf { title, kind, items });
    }
    (out, next)
}

/// "Up next" radio for a song, used for automatic suggestions.
fn parse_radio(v: &serde_json::Value) -> Vec<Track> {
    let items = &v["contents"]["singleColumnMusicWatchNextResultsRenderer"]["tabbedRenderer"]["watchNextTabbedResultsRenderer"]["tabs"][0]["tabRenderer"]["content"]
        ["musicQueueRenderer"]["content"]["playlistPanelRenderer"]["contents"];
    let mut out = Vec::new();
    for it in items.as_array().into_iter().flatten() {
        let v = &it["playlistPanelVideoRenderer"];
        let Some(id) = v["videoId"].as_str() else { continue };
        let (thumb, cover) = thumbs_of(&v["thumbnail"]["thumbnails"]);
        // "Artist • Album • 2026"
        let byline = runs(&v["longBylineText"]);
        let (artist_id, album_id) = ids_in(&v["longBylineText"]);
        out.push(Track {
            id: id.to_string(),
            title: runs(&v["title"]),
            artist: byline.split(" • ").next().unwrap_or_default().to_string(),
            album: if album_id.is_empty() { String::new() } else { byline.split(" • ").nth(1).unwrap_or_default().to_string() },
            secs: parse_dur(&runs(&v["lengthText"])).unwrap_or(0),
            thumb,
            cover,
            sug: true,
            artist_id,
            album_id,
            ..Default::default()
        });
    }
    out
}

/// Album page: every track shares the album cover.
fn parse_album(v: &serde_json::Value) -> Vec<Track> {
    let main = &v["contents"]["twoColumnBrowseResultsRenderer"];
    let hdr = &main["tabs"][0]["tabRenderer"]["content"]["sectionListRenderer"]["contents"][0]["musicResponsiveHeaderRenderer"];
    let (thumb, cover) = thumbs_of(&hdr["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]);
    let artist = runs(&hdr["straplineTextOne"]);
    let (artist_id, _) = ids_in(&hdr["straplineTextOne"]);
    let album = runs(&hdr["title"]);
    let mut out = Vec::new();
    for s in main["secondaryContents"]["sectionListRenderer"]["contents"].as_array().into_iter().flatten() {
        for it in s["musicShelfRenderer"]["contents"].as_array().into_iter().flatten() {
            let i = &it["musicResponsiveListItemRenderer"];
            let Some(id) = i["playlistItemData"]["videoId"].as_str() else { continue };
            out.push(Track {
                id: id.to_string(),
                title: list_col(i, 0),
                artist: artist.clone(),
                secs: parse_dur(&runs(&i["fixedColumns"][0]["musicResponsiveListItemFixedColumnRenderer"]["text"])).unwrap_or(0),
                thumb: thumb.clone(),
                cover: cover.clone(),
                artist_id: artist_id.clone(),
                album: album.clone(),
                ..Default::default()
            });
        }
    }
    out
}

/// Deletes cached songs not touched in the last 7 days.
fn prune_cache() {
    let Ok(rd) = std::fs::read_dir(cache_dir()) else { return };
    for e in rd.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|d| d.as_secs() > 7 * 86400);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn ytdlp_path() -> PathBuf {
    let beside_exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("bin").join("yt-dlp.exe")));
    let local = PathBuf::from("bin").join("yt-dlp.exe");
    match beside_exe {
        Some(p) if p.exists() => p,
        _ if local.exists() => local,
        _ => PathBuf::from("yt-dlp"),
    }
}

/// Installed builds (the installer drops an `installed` marker next to the exe) keep their
/// settings and session in `%LOCALAPPDATA%\Temazoo`; development runs keep using the cwd.
fn enter_data_dir() {
    let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) else { return };
    if !exe_dir.join("installed").exists() {
        return;
    }
    let root = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let data = root.join("Temazoo");
    let _ = std::fs::create_dir_all(&data);
    let _ = std::env::set_current_dir(data);
}

/// Authenticated client when `headers.json` is usable, otherwise an anonymous one.
fn build_client() -> Arc<YTMusicClient> {
    let authed = BrowserAuth::from_file("headers.json").and_then(|a| YTMusicClient::builder().with_browser_auth(a).build());
    Arc::new(authed.or_else(|_| YTMusicClient::builder().build()).expect("no se pudo crear el cliente HTTP"))
}

/// Runs this executable as a login/refresh child and returns its exit code.
/// Pids of the `--login`/`--refresh` helper processes, so they die with the app instead of lingering.
static CHILDREN: Mutex<Vec<u32>> = Mutex::new(Vec::new());

fn run_child(flag: &str) -> Option<i32> {
    let mut child = std::process::Command::new(std::env::current_exe().ok()?).arg(flag).spawn().ok()?;
    let pid = child.id();
    CHILDREN.lock().ok()?.push(pid);
    let code = child.wait().ok().and_then(|s| s.code());
    if let Ok(mut c) = CHILDREN.lock() {
        c.retain(|p| *p != pid);
    }
    code
}

fn kill_children() {
    use std::os::windows::process::CommandExt;
    let pids: Vec<u32> = CHILDREN.lock().map(|c| c.clone()).unwrap_or_default();
    for p in pids {
        let _ = std::process::Command::new("taskkill")
            .creation_flags(0x0800_0000)
            .args(["/F", "/T", "/PID", &p.to_string()])
            .output();
    }
}

/// True when `headers.json` is missing or older than 6 hours (Google rotates the tokens).
fn session_stale() -> bool {
    std::fs::metadata("headers.json")
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.elapsed().ok())
        .is_none_or(|d| d.as_secs() > 6 * 3600)
}

fn spawn_backend(ctx: egui::Context, rx: mpsc::Receiver<Cmd>, tx: mpsc::Sender<Evt>) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let client = Arc::new(RwLock::new(build_client()));
        let inflight = Arc::new(Mutex::new(HashSet::<String>::new()));
        prune_cache();
        for cmd in rx {
            if matches!(cmd, Cmd::Reauth) {
                *client.write().unwrap() = build_client();
                continue;
            }
            let (client, tx, ctx, inflight) = (client.read().unwrap().clone(), tx.clone(), ctx.clone(), inflight.clone());
            rt.spawn(async move {
                let evt = match cmd {
                    Cmd::Reauth => return, // handled before spawning
                    Cmd::Account => Evt::Account(match client.send_request("account/account_menu", serde_json::json!({})).await {
                        Ok(v) => parse_account(&v),
                        Err(_) => None,
                    }),
                    Cmd::LoadLikes => match client.get_liked_songs(Some(2000)).await {
                        Ok(p) => Evt::Likes(p.tracks.into_iter().filter_map(|t| t.video_id).collect()),
                        Err(_) => Evt::Likes(vec![]),
                    },
                    Cmd::Rate { vid, like } => {
                        let r = if like { client.like_song(&vid).await } else { client.unlike_song(&vid).await };
                        match r {
                            Ok(_) => Evt::Done { msg: if like { "Agregada a Me gusta".into() } else { "Quitada de Me gusta".into() }, reload: false },
                            Err(e) => Evt::RateFailed(vid, like, e.to_string()),
                        }
                    }
                    Cmd::Feed { idx, browse, params, token } => {
                        let first = token.is_none();
                        let res = match &token {
                            None => {
                                let mut body = serde_json::json!({ "browseId": browse });
                                if let Some(p) = &params {
                                    body["params"] = serde_json::json!(p);
                                }
                                client.send_request("browse", body).await
                            }
                            Some(t) => client.send_request(&format!("browse?ctoken={t}&continuation={t}&type=next"), serde_json::json!({})).await,
                        };
                        match res {
                            Ok(v) => {
                                let sl = if !first {
                                    &v["continuationContents"]["sectionListContinuation"]
                                } else if v["contents"]["singleColumnBrowseResultsRenderer"].is_object() {
                                    &v["contents"]["singleColumnBrowseResultsRenderer"]["tabs"][0]["tabRenderer"]["content"]["sectionListRenderer"]
                                } else {
                                    &v["contents"]["twoColumnBrowseResultsRenderer"]["secondaryContents"]["sectionListRenderer"]
                                };
                                let (shelves, next) = parse_feed(sl);
                                Evt::Feed { idx, shelves, next, first }
                            }
                            Err(e) => Evt::Error(format!("explorar: {e}")),
                        }
                    }
                    Cmd::CheckOwned(ids) => {
                        let mut set = tokio::task::JoinSet::new();
                        for id in ids {
                            let c = client.clone();
                            set.spawn(async move { c.get_playlist(&id, Some(1)).await.ok().map(|p| (id, p.owned)) });
                        }
                        let (mut owned, mut checked) = (Vec::new(), Vec::new());
                        while let Some(r) = set.join_next().await {
                            if let Ok(Some((id, o))) = r {
                                checked.push(id.clone());
                                if o {
                                    owned.push(id);
                                }
                            }
                        }
                        Evt::Owned(owned, checked)
                    }
                    Cmd::LoadEdit(id) => match client.get_playlist(&id, Some(1)).await {
                        Ok(p) => {
                            let privacy = match p.privacy {
                                Privacy::Public => Priv::Public,
                                Privacy::Unlisted => Priv::Unlisted,
                                Privacy::Private => Priv::Private,
                            };
                            Evt::EditInfo(id, p.title, p.description.unwrap_or_default(), privacy)
                        }
                        Err(e) => Evt::Error(format!("playlist: {e}")),
                    },
                    Cmd::Create { title, desc, privacy, with } => {
                        let p = match privacy {
                            Priv::Public => Privacy::Public,
                            Priv::Unlisted => Privacy::Unlisted,
                            Priv::Private => Privacy::Private,
                        };
                        match client.create_playlist(&title, Some(&desc), p).await {
                            Ok(r) => {
                                if let Some(v) = with {
                                    let _ = client.add_playlist_items(&r.playlist_id, &[v], false).await;
                                }
                                Evt::Created(r.playlist_id)
                            }
                            Err(e) => Evt::Error(format!("crear playlist: {e}")),
                        }
                    }
                    Cmd::Edit { id, title, desc, privacy } => {
                        let body = serde_json::json!({ "playlistId": id, "actions": [
                            { "action": "ACTION_SET_PLAYLIST_NAME", "playlistName": title },
                            { "action": "ACTION_SET_PLAYLIST_DESCRIPTION", "playlistDescription": desc },
                            { "action": "ACTION_SET_PLAYLIST_PRIVACY", "playlistPrivacy": privacy.api() },
                        ]});
                        match client.send_request("browse/edit_playlist", body).await {
                            Ok(v) if v["status"] == "STATUS_SUCCEEDED" => Evt::Done { msg: "Playlist actualizada".into(), reload: true },
                            Ok(_) => Evt::Error("no se pudo editar la playlist".into()),
                            Err(e) => Evt::Error(format!("editar playlist: {e}")),
                        }
                    }
                    Cmd::DeletePl(id) => match client.delete_playlist(&id).await {
                        Ok(()) => Evt::Done { msg: "Playlist eliminada".into(), reload: true },
                        Err(e) => Evt::Error(format!("eliminar playlist: {e}")),
                    },
                    Cmd::AddTo { id, name, vid } => match client.add_playlist_items(&id, &[vid], false).await {
                        Ok(v) if v["status"] == "STATUS_SUCCEEDED" => Evt::Done { msg: format!("Agregada a «{name}»"), reload: false },
                        Ok(_) => Evt::Error(format!("no se pudo agregar a «{name}» (¿ya está en la playlist?)")),
                        Err(e) => Evt::Error(format!("agregar: {e}")),
                    },
                    Cmd::RemoveFrom { id, vid, set } => {
                        let item = ytmusicapi::PlaylistTrack {
                            video_id: Some(vid),
                            title: None,
                            artists: vec![],
                            album: None,
                            duration: None,
                            duration_seconds: None,
                            thumbnails: vec![],
                            is_available: true,
                            is_explicit: false,
                            set_video_id: Some(set.clone()),
                            video_type: None,
                        };
                        match client.remove_playlist_items(&id, &[item]).await {
                            Ok(_) => Evt::Removed(set),
                            Err(e) => Evt::Error(format!("quitar: {e}")),
                        }
                    }
                    Cmd::Playlists => match client.get_library_playlists(Some(200)).await {
                        Ok(p) => Evt::Playlists(p.into_iter().map(|x| (x.playlist_id, x.title)).collect()),
                        Err(e) => Evt::Error(format!("playlists: {e}")),
                    },
                    // The crate has no search/browse wrappers, so use the raw endpoints.
                    Cmd::Search(q, kind) => {
                        let body = serde_json::json!({ "query": q, "params": kind.params() });
                        match client.send_request("search", body).await {
                            Ok(v) if kind.is_tracks() => Evt::Results(q, kind, parse_search(&v), vec![]),
                            Ok(v) => {
                                let mut cards = parse_search_cards(&v, kind);
                                if kind == Kind::Playlists {
                                    // Editorial playlists ("YouTube Music") come from a different filter; list them first.
                                    let body = serde_json::json!({ "query": q, "params": "EgeKAQQoADgBagwQDhAKEAMQBBAJEAU%3D" });
                                    if let Ok(f) = client.send_request("search", body).await {
                                        let mut featured = parse_search_cards(&f, kind);
                                        featured.retain(|c| !cards.iter().any(|x| x.id == c.id));
                                        featured.extend(cards);
                                        cards = featured;
                                    }
                                }
                                Evt::Results(q, kind, vec![], cards)
                            }
                            Err(e) => Evt::Error(format!("búsqueda: {e}")),
                        }
                    }
                    Cmd::Artist(id) => match client.send_request("browse", serde_json::json!({ "browseId": id })).await {
                        Ok(v) => {
                            let (name, tracks, mut cards, more) = parse_artist(&v);
                            // The page only carries a preview of each carousel: load the full list.
                            for (section, browse, params) in carousel_more(&v) {
                                let mut body = serde_json::json!({ "browseId": browse });
                                if let Some(p) = params {
                                    body["params"] = p.into();
                                }
                                let Ok(full) = client.send_request("browse", body).await else { continue };
                                let full = parse_full_cards(&full, &section);
                                if full.len() > cards.iter().filter(|c| c.section == section).count() {
                                    let at = cards.iter().position(|c| c.section == section).unwrap_or(cards.len());
                                    cards.retain(|c| c.section != section);
                                    let at = at.min(cards.len());
                                    cards.splice(at..at, full);
                                }
                            }
                            Evt::Artist(name, tracks, cards, more)
                        }
                        Err(e) => Evt::Error(format!("artista: {e}")),
                    },
                    Cmd::Suggest(input) => match client.send_request("music/get_search_suggestions", serde_json::json!({ "input": input })).await {
                        Ok(v) => {
                            let mut out = Vec::new();
                            for sec in v["contents"].as_array().into_iter().flatten() {
                                for it in sec["searchSuggestionsSectionRenderer"]["contents"].as_array().into_iter().flatten() {
                                    let s = it.get("searchSuggestionRenderer").or_else(|| it.get("historySuggestionRenderer"));
                                    if let Some(q) = s.and_then(|s| s["navigationEndpoint"]["searchEndpoint"]["query"].as_str()) {
                                        if !out.iter().any(|o| o == q) {
                                            out.push(q.to_string());
                                        }
                                    }
                                }
                            }
                            out.truncate(8);
                            Evt::Suggest(input, out)
                        }
                        // Autocomplete is a nicety: never show an error for it.
                        Err(_) => return,
                    },
                    // Full song list of an artist ("see all" of the top songs).
                    Cmd::MoreSongs(browse, params) => {
                        let mut body = serde_json::json!({ "browseId": browse });
                        if let Some(p) = params {
                            body["params"] = p.into();
                        }
                        match client.send_request("browse", body).await {
                            Ok(v) => Evt::Tracks(parse_shelf_songs(&v)),
                            Err(e) => Evt::Error(format!("canciones del artista: {e}")),
                        }
                    }
                    Cmd::Video(id, max_h) => {
                        // Songs from the catalogue ("ATV") are audio only: just ask, don't download anything.
                        let body = serde_json::json!({ "videoId": id, "isAudioOnly": true });
                        let has_video = match client.send_request("next", body).await {
                            Ok(v) => find_key(&v, "musicVideoType")
                                .and_then(|t| t.as_str())
                                .is_some_and(|t| matches!(t, "MUSIC_VIDEO_TYPE_OMV" | "MUSIC_VIDEO_TYPE_UGC" | "MUSIC_VIDEO_TYPE_OFFICIAL_SOURCE_MUSIC")),
                            Err(_) => false,
                        };
                        let path = cache_dir().join(format!("{id}.video.mp4"));
                        // Keep only the current video on disk (they are big).
                        if let Ok(rd) = std::fs::read_dir(cache_dir()) {
                            let mine = format!("{id}.");
                            for f in rd.flatten().filter(|f| {
                                let n = f.file_name().to_string_lossy().to_string();
                                (n.ends_with(".video.mp4") || n.ends_with(".video.mp4.part")) && !n.starts_with(&mine)
                            }) {
                                let _ = std::fs::remove_file(f.path());
                            }
                        }
                        if has_video && !path.exists() {
                            let mut c = tokio::process::Command::new(ytdlp_path());
                            c.args(["-f", &format!("bv*[vcodec^=avc1][height<={max_h}]/worst[vcodec^=avc1]"), "--no-playlist", "-q", "--no-warnings", "-o"])
                                .arg(cache_dir().join(format!("{id}.video.%(ext)s")))
                                .arg(format!("https://music.youtube.com/watch?v={id}"));
                            #[cfg(windows)]
                            c.creation_flags(0x08000000);
                            let _ = c.output().await;
                        }
                        Evt::Video(id, (has_video && path.exists()).then_some(path))
                    }
                    Cmd::Lyrics(id, title, artist, secs) => {
                        // Synced lyrics first (LRCLIB). Otherwise the plain text YouTube Music keeps behind a tab
                        // of the "next" response: find its browse id, then fetch it.
                        let found = if let Some(lines) = lrclib_synced(&title, &artist, secs).await {
                            Some(Lyrics::Synced { lines, source: "LRCLIB".into() })
                        } else {
                            async {
                                let n = client.send_request("next", serde_json::json!({ "videoId": id, "isAudioOnly": true })).await.ok()?;
                                let browse = lyrics_browse_id(&n)?;
                                let l = client.send_request("browse", serde_json::json!({ "browseId": browse })).await.ok()?;
                                let shelf = find_key(&l, "musicDescriptionShelfRenderer")?;
                                let text = runs(&shelf["description"]);
                                let source = runs(&shelf["footer"]).trim_start_matches("Source: ").to_string();
                                (!text.trim().is_empty()).then_some(Lyrics::Plain { text, source })
                            }
                            .await
                        };
                        Evt::Lyrics(id, found)
                    }
                    Cmd::VideoHd(id, want, have) => {
                        let sel = format!("bv*[vcodec^=avc1][height<={}]", want.clamp(240, 1080));
                        let url = format!("https://music.youtube.com/watch?v={id}");
                        // Ask yt-dlp which height that selector gives (no download) and only fetch if it is better.
                        let mut c = tokio::process::Command::new(ytdlp_path());
                        c.args(["-f", &sel, "--no-playlist", "-q", "--no-warnings", "--print", "%(height)s", &url]);
                        #[cfg(windows)]
                        c.creation_flags(0x08000000);
                        let best: u32 = match c.output().await {
                            Ok(o) => String::from_utf8_lossy(&o.stdout).trim().parse().unwrap_or(0),
                            Err(_) => 0,
                        };
                        let path = cache_dir().join(format!("{id}.hd{best}.video.mp4"));
                        if best > have && !path.exists() {
                            let mut c = tokio::process::Command::new(ytdlp_path());
                            c.args(["-f", &sel, "--no-playlist", "-q", "--no-warnings", "-o"])
                                .arg(cache_dir().join(format!("{id}.hd{best}.video.%(ext)s")))
                                .arg(&url);
                            #[cfg(windows)]
                            c.creation_flags(0x08000000);
                            let _ = c.output().await;
                        }
                        Evt::VideoHd(id, (best > have && path.exists()).then_some(path))
                    }
                    Cmd::Radio(id) => {
                        let body = serde_json::json!({ "videoId": id, "playlistId": format!("RDAMVM{id}"), "isAudioOnly": true });
                        match client.send_request("next", body).await {
                            Ok(v) => Evt::Radio(parse_radio(&v)),
                            Err(e) => Evt::Error(format!("sugerencias: {e}")),
                        }
                    }
                    Cmd::Album(id) => match client.send_request("browse", serde_json::json!({ "browseId": id })).await {
                        Ok(v) => Evt::Tracks(parse_album(&v).into_iter().map(|t| Track { album_id: id.clone(), ..t }).collect()),
                        Err(e) => Evt::Error(format!("álbum: {e}")),
                    },
                    Cmd::Tracks(id) => {
                        let r = if id == "LM" {
                            client.get_liked_songs(Some(1000)).await
                        } else {
                            client.get_playlist(&id, Some(1000)).await
                        };
                        match r {
                            Ok(p) => Evt::Tracks(
                                p.tracks
                                    .into_iter()
                                    .filter(|t| t.is_available)
                                    .filter_map(|t| {
                                        Some(Track {
                                            id: t.video_id?,
                                            title: t.title.unwrap_or_default().replace('\u{a0}', " "),
                                            artist: t.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", "),
                                            secs: t.duration_seconds.or_else(|| t.duration.as_deref().and_then(parse_dur)).unwrap_or(0),
                                            thumb: t.thumbnails.first().map(|x| resize(&x.url, 96)).unwrap_or_default(),
                                            cover: t.thumbnails.last().map(|x| resize(&x.url, 256)).unwrap_or_default(),
                                            svid: t.set_video_id.clone().unwrap_or_default(),
                                            artist_id: t.artists.iter().find_map(|a| a.id.clone()).unwrap_or_default(),
                                            album_id: t.album.as_ref().and_then(|a| a.id.clone()).unwrap_or_default(),
                                            album: t.album.as_ref().map(|a| a.name.clone()).unwrap_or_default(),
                                            ..Default::default()
                                        })
                                    })
                                    .collect(),
                            ),
                            Err(e) => Evt::Error(format!("tracks: {e}")),
                        }
                    }
                    Cmd::Fetch(t, prefetch, q) => {
                        let r = if let Some(path) = cached(&t.id, q) {
                            Evt::Ready(t.id.clone(), path)
                        } else if !inflight.lock().unwrap().insert(t.id.clone()) {
                            // Already being downloaded (e.g. by a prefetch): wait for it.
                            while inflight.lock().unwrap().contains(&t.id) {
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                            match downloaded(&t.id, q) {
                                Some(path) => Evt::Ready(t.id.clone(), path),
                                None => Evt::Error("la descarga falló".into()),
                            }
                        } else {
                            let mut c = tokio::process::Command::new(ytdlp_path());
                            c.args(["-f", q.yt_format(), "--no-playlist", "-q", "--no-warnings", "-o"])
                                .arg(cache_dir().join(format!("{}.%(ext)s", t.id)))
                                .arg(format!("https://music.youtube.com/watch?v={}", t.id));
                            #[cfg(windows)]
                            c.creation_flags(0x08000000);
                            let out = c.output().await;
                            inflight.lock().unwrap().remove(&t.id);
                            match (out, downloaded(&t.id, q)) {
                                (Ok(o), Some(path)) if o.status.success() => Evt::Ready(t.id.clone(), path),
                                (Ok(o), _) => Evt::Error(format!("yt-dlp: {}", String::from_utf8_lossy(&o.stderr).trim())),
                                (Err(e), _) => Evt::Error(format!("yt-dlp no se pudo ejecutar: {e}")),
                            }
                        };
                        if prefetch {
                            Evt::Prefetched
                        } else {
                            r
                        }
                    }
                };
                let _ = tx.send(evt);
                ctx.request_repaint();
            });
        }
    });
}

fn spawn_audio(rx: mpsc::Receiver<Audio>, state: Arc<Mutex<AudioState>>) {
    std::thread::spawn(move || {
        let Ok(sink) = DeviceSinkBuilder::open_default_sink() else {
            eprintln!("no hay dispositivo de audio");
            return;
        };
        let player = Player::connect_new(sink.mixer());
        let mut loaded = false;
        let mut vol = 0.7f32;
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Audio::Load(p)) => {
                    player.clear();
                    let mut total = Duration::ZERO;
                    match decode::SymSource::open(&p) {
                        Ok(d) => {
                            total = d.total_duration().unwrap_or_default();
                            player.append(d);
                            player.play();
                            loaded = true;
                        }
                        Err(e) => eprintln!("decodificación: {e}"),
                    }
                    let mut s = state.lock().unwrap();
                    s.serial += 1;
                    s.total = total;
                    s.finished = !loaded;
                }
                Ok(Audio::Pause) => player.pause(),
                Ok(Audio::Resume) => player.play(),
                Ok(Audio::Volume(v)) => {
                    vol = v;
                    player.set_volume(v);
                }
                Ok(Audio::Fade) => {
                    for k in (0..8).rev() {
                        player.set_volume(vol * k as f32 / 8.0);
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    player.clear();
                    player.set_volume(vol);
                    loaded = false;
                    let mut s = state.lock().unwrap();
                    s.pos = Duration::ZERO;
                    s.finished = false;
                }
                Ok(Audio::Seek(d)) => {
                    let _ = player.try_seek(d);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => return,
            }
            let mut s = state.lock().unwrap();
            s.pos = player.get_pos();
            s.at = Some(std::time::Instant::now());
            if loaded && player.empty() {
                s.finished = true;
                loaded = false;
            }
        }
    });
}

fn resize(url: &str, n: u32) -> String {
    match url.rfind("=w") {
        Some(i) if url.contains("googleusercontent") || url.contains("ggpht") => format!("{}=w{n}-h{n}-l90-rj", &url[..i]),
        _ => url.to_string(),
    }
}

const ACCENT: Color32 = Color32::from_rgb(167, 139, 250);
const SIDEBAR: Color32 = Color32::from_rgb(12, 12, 17);
const BG: Color32 = Color32::from_rgb(20, 20, 27);
const BAR: Color32 = Color32::from_rgb(26, 26, 35);
const DIM: Color32 = Color32::from_gray(140);

#[derive(Clone, Copy)]
enum Icon {
    Prev,
    Next,
    Play,
    Pause,
}

fn icon_button(ui: &mut egui::Ui, icon: Icon, size: f32, filled: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let hover = resp.hovered();
    let p = ui.painter();
    let c = rect.center();
    if filled {
        p.circle_filled(c, size / 2.0, if hover { Color32::WHITE } else { Color32::from_gray(225) });
    }
    let fg = match (filled, hover) {
        (true, _) => Color32::BLACK,
        (false, true) => Color32::WHITE,
        _ => Color32::from_gray(170),
    };
    let s = size * if filled { 0.22 } else { 0.26 };
    let tri = |dir: f32, off: f32| {
        egui::Shape::convex_polygon(
            vec![
                egui::pos2(c.x + off - dir * s * 0.8, c.y - s),
                egui::pos2(c.x + off + dir * s * 0.8, c.y),
                egui::pos2(c.x + off - dir * s * 0.8, c.y + s),
            ],
            fg,
            egui::Stroke::NONE,
        )
    };
    let bar = |x: f32| egui::Rect::from_center_size(egui::pos2(x, c.y), egui::vec2(s * 0.45, s * 2.0));
    match icon {
        Icon::Play => {
            p.add(tri(1.0, s * 0.15));
        }
        Icon::Pause => {
            p.rect_filled(bar(c.x - s * 0.5), 1.0, fg);
            p.rect_filled(bar(c.x + s * 0.5), 1.0, fg);
        }
        Icon::Prev => {
            p.rect_filled(bar(c.x - s * 0.9), 1.0, fg);
            p.add(tri(-1.0, s * 0.4));
        }
        Icon::Next => {
            p.rect_filled(bar(c.x + s * 0.9), 1.0, fg);
            p.add(tri(1.0, -s * 0.4));
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[derive(Clone, Copy, PartialEq)]
enum SettingsTab {
    Playback,
    Video,
    Lyrics,
    Account,
    Storage,
    About,
}

/// Left-hand navigation entry of the settings window.
fn nav_item(ui: &mut egui::Ui, label: &str, active: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 34.0), egui::Sense::click());
    if active {
        ui.painter().rect_filled(rect, 8.0, Color32::from_rgba_unmultiplied(167, 139, 250, 36));
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 8.0, Color32::from_white_alpha(10));
    }
    ui.painter().text(
        rect.left_center() + egui::vec2(12.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(14.5),
        if active { ACCENT } else { Color32::from_gray(210) },
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn page_title(ui: &mut egui::Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new(text).strong().size(20.0));
    ui.add_space(10.0);
}

/// A rounded block that groups related settings.
fn settings_card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new().fill(Color32::from_gray(24)).corner_radius(10.0).inner_margin(egui::Margin::symmetric(14, 12)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui);
    });
    ui.add_space(12.0);
}

fn card_divider(ui: &mut egui::Ui) {
    ui.add_space(8.0);
    ui.separator();
    ui.add_space(8.0);
}

/// "Title + explanation" on the left, the control on the right.
fn setting_row(ui: &mut egui::Ui, title: &str, desc: &str, control: impl FnOnce(&mut egui::Ui)) {
    // Title on the left and the control on the right; the explanation goes below, using the full width,
    // so a wide control (four buttons) never collides with it.
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(title).size(14.5));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control);
    });
    if !desc.is_empty() {
        ui.add(egui::Label::new(egui::RichText::new(desc).color(DIM).size(12.0)).wrap());
    }
}

/// A row of exclusive options. Returns the newly picked index. (Drawn right to left, so it is added
/// in reverse to read left to right inside `setting_row`.)
fn segmented(ui: &mut egui::Ui, options: &[&str], selected: usize) -> Option<usize> {
    let mut pick = None;
    for (i, o) in options.iter().enumerate().rev() {
        if chip(ui, o, i == selected).clicked() && i != selected {
            pick = Some(i);
        }
    }
    pick
}

fn switch(ui: &mut egui::Ui, on: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(34.0, 18.0), egui::Sense::click());
    ui.painter().rect_filled(rect, 9.0, if on { ACCENT } else { Color32::from_gray(70) });
    let x = if on { rect.right() - 9.0 } else { rect.left() + 9.0 };
    ui.painter().circle_filled(egui::pos2(x, rect.center().y), 6.5, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Draws the "three dots" button of a row. Returns where the menu should open if the user
/// clicked it or right-clicked the row.
fn row_menu_trigger(ui: &mut egui::Ui, rect: egui::Rect, row: &egui::Response) -> Option<egui::Pos2> {
    let area = egui::Rect::from_center_size(egui::pos2(rect.right() - 18.0, rect.center().y), egui::vec2(26.0, 26.0));
    let d = ui.interact(area, row.id.with("dots"), egui::Sense::click());
    if row.hovered() || d.hovered() {
        dots(ui, area.center(), if d.hovered() { Color32::WHITE } else { DIM });
    }
    if d.clicked() {
        return Some(area.left_bottom());
    }
    if row.secondary_clicked() {
        return row.interact_pointer_pos();
    }
    None
}

/// Four corner brackets: pointing outwards to enter fullscreen, inwards to leave it.
fn fs_glyph(p: &egui::Painter, c: egui::Pos2, col: Color32, exit: bool) {
    let s = egui::Stroke::new(1.8, col);
    let a = 5.0; // arm length
    for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
        if exit {
            // Brackets near the centre with the arms pointing outwards.
            let corner = egui::pos2(c.x + sx * 3.0, c.y + sy * 3.0);
            p.line_segment([corner, egui::pos2(corner.x + sx * a, corner.y)], s);
            p.line_segment([corner, egui::pos2(corner.x, corner.y + sy * a)], s);
        } else {
            // Brackets in the four outer corners, arms along the edges.
            let corner = egui::pos2(c.x + sx * 7.0, c.y + sy * 7.0);
            p.line_segment([corner, egui::pos2(corner.x - sx * a, corner.y)], s);
            p.line_segment([corner, egui::pos2(corner.x, corner.y - sy * a)], s);
        }
    }
}

fn fs_button(ui: &mut egui::Ui, on: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
    let col = if on { ACCENT } else if resp.hovered() { Color32::WHITE } else { Color32::from_gray(170) };
    fs_glyph(ui.painter(), rect.center(), col, on);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[derive(Clone, Copy, PartialEq)]
enum RightTab {
    Queue,
    Lyrics,
}

enum Lyrics {
    Loading,
    /// Plain text (YouTube Music).
    Plain { text: String, source: String },
    /// Lines with their start time in ms (LRCLIB), to follow the song.
    Synced { lines: Vec<(u32, String)>, source: String },
    Missing,
}

#[derive(Clone, Copy, PartialEq)]
enum Repeat {
    Off,
    All,
    One,
}

/// Crossed arrows. Lit when shuffle is on.
fn shuffle_button(ui: &mut egui::Ui, on: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::click());
    let col = if on { ACCENT } else if resp.hovered() { Color32::WHITE } else { Color32::from_gray(150) };
    let (c, p) = (rect.center(), ui.painter());
    let s = egui::Stroke::new(1.8, col);
    let (l, r, t, b) = (c.x - 8.0, c.x + 6.0, c.y - 5.0, c.y + 5.0);
    p.line_segment([egui::pos2(l, t), egui::pos2(c.x - 2.0, t)], s);
    p.line_segment([egui::pos2(c.x - 2.0, t), egui::pos2(c.x + 2.0, b)], s);
    p.line_segment([egui::pos2(c.x + 2.0, b), egui::pos2(r, b)], s);
    p.line_segment([egui::pos2(l, b), egui::pos2(c.x - 2.0, b)], s);
    p.line_segment([egui::pos2(c.x - 2.0, b), egui::pos2(c.x + 2.0, t)], s);
    p.line_segment([egui::pos2(c.x + 2.0, t), egui::pos2(r, t)], s);
    for y in [t, b] {
        p.add(egui::Shape::convex_polygon(
            vec![egui::pos2(r + 4.0, y), egui::pos2(r - 1.0, y - 3.0), egui::pos2(r - 1.0, y + 3.0)],
            col,
            egui::Stroke::NONE,
        ));
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Loop arrows; a "1" inside for repeat-one. Lit when repeating.
fn repeat_button(ui: &mut egui::Ui, mode: Repeat) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::click());
    let on = mode != Repeat::Off;
    let col = if on { ACCENT } else if resp.hovered() { Color32::WHITE } else { Color32::from_gray(150) };
    let (c, p) = (rect.center(), ui.painter());
    let s = egui::Stroke::new(1.8, col);
    let (l, r, t, b) = (c.x - 8.0, c.x + 8.0, c.y - 5.5, c.y + 5.5);
    p.line_segment([egui::pos2(l + 2.0, t), egui::pos2(r - 2.0, t)], s);
    p.line_segment([egui::pos2(r - 2.0, t), egui::pos2(r, t + 2.0)], s);
    p.line_segment([egui::pos2(r, t + 2.0), egui::pos2(r, b - 2.0)], s);
    p.line_segment([egui::pos2(l + 2.0, b), egui::pos2(r - 2.0, b)], s);
    p.line_segment([egui::pos2(l, b - 2.0), egui::pos2(l + 2.0, b)], s);
    p.line_segment([egui::pos2(l, t + 2.0), egui::pos2(l, b - 2.0)], s);
    p.line_segment([egui::pos2(l, t + 2.0), egui::pos2(l + 2.0, t)], s);
    // arrow heads on the top-right and bottom-left corners
    p.add(egui::Shape::convex_polygon(
        vec![egui::pos2(r - 6.0, t - 3.5), egui::pos2(r - 6.0, t + 3.5), egui::pos2(r - 1.5, t)],
        col,
        egui::Stroke::NONE,
    ));
    p.add(egui::Shape::convex_polygon(
        vec![egui::pos2(l + 6.0, b - 3.5), egui::pos2(l + 6.0, b + 3.5), egui::pos2(l + 1.5, b)],
        col,
        egui::Stroke::NONE,
    ));
    if mode == Repeat::One {
        p.text(c, egui::Align2::CENTER_CENTER, "1", egui::FontId::proportional(10.0), col);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn queue_button(ui: &mut egui::Ui, active: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
    let col = if active { ACCENT } else if resp.hovered() { Color32::WHITE } else { Color32::from_gray(170) };
    let (c, p) = (rect.center(), ui.painter());
    let s = egui::Stroke::new(2.0, col);
    for k in 0..3 {
        let y = c.y - 6.0 + k as f32 * 6.0;
        let end = if k == 0 { c.x + 3.0 } else { c.x + 5.0 };
        p.line_segment([egui::pos2(c.x - 7.0, y), egui::pos2(end, y)], s);
    }
    p.add(egui::Shape::convex_polygon(
        vec![egui::pos2(c.x + 5.0, c.y - 9.0), egui::pos2(c.x + 5.0, c.y - 3.0), egui::pos2(c.x + 10.0, c.y - 6.0)],
        col,
        egui::Stroke::NONE,
    ));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn dots(ui: &egui::Ui, c: egui::Pos2, col: Color32) {
    for dy in [-5.0, 0.0, 5.0] {
        ui.painter().circle_filled(c + egui::vec2(0.0, dy), 1.9, col);
    }
}

/// Heart: filled pink when liked, outlined when not.
fn heart_button(ui: &mut egui::Ui, liked: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(26.0, 26.0), egui::Sense::click());
    let col = if liked {
        Color32::from_rgb(255, 82, 112)
    } else if resp.hovered() {
        Color32::WHITE
    } else {
        Color32::from_gray(150)
    };
    let heart = |ui: &egui::Ui, c: egui::Pos2, r: f32, col: Color32| {
        ui.painter().circle_filled(c + egui::vec2(-r * 0.95, -r * 0.55), r, col);
        ui.painter().circle_filled(c + egui::vec2(r * 0.95, -r * 0.55), r, col);
        ui.painter().add(egui::Shape::convex_polygon(
            vec![egui::pos2(c.x - r * 1.8, c.y - r * 0.15), egui::pos2(c.x + r * 1.8, c.y - r * 0.15), egui::pos2(c.x, c.y + r * 2.0)],
            col,
            egui::Stroke::NONE,
        ));
    };
    let c = rect.center() + egui::vec2(0.0, 0.5);
    heart(ui, c, 5.2, col);
    if !liked {
        heart(ui, c + egui::vec2(0.0, 0.6), 3.0, BAR);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Four-arrow "move" icon shown on queue rows.
fn move_icon(ui: &egui::Ui, c: egui::Pos2, col: Color32) {
    let (s, l) = (egui::Stroke::new(1.6, col), 7.0);
    ui.painter().line_segment([c + egui::vec2(-l, 0.0), c + egui::vec2(l, 0.0)], s);
    ui.painter().line_segment([c + egui::vec2(0.0, -l), c + egui::vec2(0.0, l)], s);
    for (dx, dy) in [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)] {
        let dir = egui::vec2(dx, dy);
        let (tip, base, perp) = (c + dir * (l + 2.5), c + dir * (l - 1.0), egui::vec2(-dy, dx) * 3.0);
        ui.painter().add(egui::Shape::convex_polygon(vec![tip, base + perp, base - perp], col, egui::Stroke::NONE));
    }
}

fn plus_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::click());
    let col = if resp.hovered() { Color32::WHITE } else { DIM };
    let (c, d) = (rect.center(), 6.0);
    let s = egui::Stroke::new(2.0, col);
    ui.painter().line_segment([c - egui::vec2(d, 0.0), c + egui::vec2(d, 0.0)], s);
    ui.painter().line_segment([c - egui::vec2(0.0, d), c + egui::vec2(0.0, d)], s);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn menu_item(ui: &mut egui::Ui, label: &str) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(220.0, 30.0), egui::Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 6.0, Color32::from_rgba_unmultiplied(167, 139, 250, 40));
    }
    ui.painter().with_clip_rect(rect.shrink2(egui::vec2(6.0, 0.0))).text(
        egui::pos2(rect.left() + 10.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(14.0),
        Color32::from_gray(225),
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

fn chip(ui: &mut egui::Ui, label: &str, active: bool) -> egui::Response {
    let col = if active { Color32::BLACK } else { Color32::from_gray(205) };
    let galley = ui.painter().layout_no_wrap(label.to_string(), egui::FontId::proportional(13.0), col);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + egui::vec2(24.0, 10.0), egui::Sense::click());
    let bg = if active {
        ACCENT
    } else if resp.hovered() {
        Color32::from_gray(52)
    } else {
        Color32::from_gray(34)
    };
    ui.painter().rect_filled(rect, 16.0, bg);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, col);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn magnifier(ui: &egui::Ui, c: egui::Pos2, r: f32, col: Color32) {
    let stroke = egui::Stroke::new(r * 0.24, col);
    let ctr = c - egui::vec2(r * 0.12, r * 0.12);
    ui.painter().circle_stroke(ctr, r * 0.55, stroke);
    let d = egui::vec2(1.0, 1.0).normalized();
    ui.painter().line_segment([ctr + d * r * 0.55, ctr + d * r * 0.95], stroke);
}

fn clear_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::click());
    let col = if resp.hovered() { Color32::WHITE } else { DIM };
    let (c, d) = (rect.center(), 4.5);
    let s = egui::Stroke::new(1.8, col);
    ui.painter().line_segment([c + egui::vec2(-d, -d), c + egui::vec2(d, d)], s);
    ui.painter().line_segment([c + egui::vec2(-d, d), c + egui::vec2(d, -d)], s);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn gear_button(ui: &mut egui::Ui, size: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let col = if resp.hovered() { Color32::WHITE } else { Color32::from_gray(170) };
    let (c, r) = (rect.center(), size / 2.0);
    let p = ui.painter();
    for k in 0..8 {
        let a = k as f32 * std::f32::consts::TAU / 8.0;
        let d = egui::vec2(a.cos(), a.sin());
        p.line_segment([c + d * r * 0.55, c + d * r * 0.95], egui::Stroke::new(size * 0.17, col));
    }
    p.circle_stroke(c, r * 0.58, egui::Stroke::new(size * 0.13, col));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Thin custom slider. Returns the response and the fraction under the pointer (or `frac`).
fn thin_bar(ui: &mut egui::Ui, width: f32, frac: f32) -> (egui::Response, f32) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 16.0), egui::Sense::click_and_drag());
    let mut f = frac;
    if resp.dragged() || resp.clicked() || resp.drag_stopped() {
        if let Some(pp) = resp.interact_pointer_pos() {
            f = ((pp.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        }
    }
    let track = egui::Rect::from_center_size(rect.center(), egui::vec2(rect.width(), 4.0));
    let fill = egui::Rect::from_min_max(track.min, egui::pos2(track.left() + track.width() * f, track.bottom()));
    let p = ui.painter();
    p.rect_filled(track, 2.0, Color32::from_gray(55));
    let active = resp.hovered() || resp.dragged();
    p.rect_filled(fill, 2.0, if active { ACCENT } else { Color32::from_gray(210) });
    if active {
        p.circle_filled(egui::pos2(fill.right(), track.center().y), 6.0, Color32::WHITE);
    }
    (resp.on_hover_cursor(egui::CursorIcon::PointingHand), f)
}

/// Images shown so far, oldest first. egui keeps every decoded image forever, so we forget
/// the oldest ones that are not on screen once there are too many (memory was growing to ~1 GB).
struct ImgLru {
    seen: HashSet<String>,
    order: std::collections::VecDeque<String>,
    frame: HashSet<String>,
}

static IMGS: std::sync::LazyLock<Mutex<ImgLru>> =
    std::sync::LazyLock::new(|| Mutex::new(ImgLru { seen: HashSet::new(), order: Default::default(), frame: HashSet::new() }));

const IMG_MAX: usize = 220;
const IMG_KEEP: usize = 140;

fn touch_image(url: &str) {
    let mut g = IMGS.lock().unwrap();
    if !g.frame.contains(url) {
        g.frame.insert(url.to_string());
    }
    if !g.seen.contains(url) {
        g.seen.insert(url.to_string());
        g.order.push_back(url.to_string());
    }
}

/// Call once per frame, after drawing: frees the oldest images that are not visible.
fn evict_images(ctx: &egui::Context) {
    let mut g = IMGS.lock().unwrap();
    if g.order.len() > IMG_MAX {
        let mut visible = std::collections::VecDeque::new();
        while g.order.len() + visible.len() > IMG_KEEP {
            let Some(u) = g.order.pop_front() else { break };
            if g.frame.contains(&u) {
                visible.push_back(u);
            } else {
                ctx.forget_image(&u);
                g.seen.remove(&u);
            }
        }
        g.order.extend(visible);
    }
    g.frame.clear();
}

fn cover(ui: &mut egui::Ui, url: &str, rect: egui::Rect, radius: f32) {
    ui.painter().rect_filled(rect, radius, Color32::from_gray(38));
    if !url.is_empty() {
        touch_image(url);
        let mut img = egui::Image::new(url.to_owned()).corner_radius(radius).show_loading_spinner(false);
        // Video thumbnails are 16:9: show their centre so they fill a square.
        if url.contains("i.ytimg.com") {
            img = img.uv(egui::Rect::from_min_max(egui::pos2(0.21875, 0.0), egui::pos2(0.78125, 1.0)));
        }
        img.paint_at(ui, rect);
    }
}

/// A song row used in "Quick picks"-style shelves.
fn song_row(ui: &mut egui::Ui, t: &Track, w: f32, is_cur: bool, hand: &mut bool, go_artist: &mut bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 52.0), egui::Sense::click());
    let hovered = ui.rect_contains_pointer(rect);
    *hand |= hovered;
    if is_cur || hovered {
        ui.painter().rect_filled(rect, 10.0, Color32::from_rgba_unmultiplied(167, 139, 250, if is_cur { 38 } else { 14 }));
    }
    cover(ui, &t.thumb, egui::Rect::from_min_size(rect.min + egui::vec2(8.0, 6.0), egui::vec2(40.0, 40.0)), 6.0);
    let clip = egui::Rect::from_min_size(rect.min + egui::vec2(58.0, 0.0), egui::vec2(w - 66.0, 52.0));
    let p = ui.painter().with_clip_rect(clip);
    p.text(egui::pos2(clip.left(), rect.center().y - 9.0), egui::Align2::LEFT_CENTER, &t.title, egui::FontId::proportional(14.5), if is_cur { ACCENT } else { Color32::WHITE });
    *go_artist = artist_line(ui, &resp, clip, rect.center().y + 10.0, t, 12.5);
    resp
}

/// Draws the artist line of a song row. When the artist is known it turns into a link on hover;
/// returns true when it was clicked (the row must not start playing then).
fn artist_line(ui: &mut egui::Ui, row: &egui::Response, clip: egui::Rect, y: f32, t: &Track, size: f32) -> bool {
    let font = egui::FontId::proportional(size);
    let linked = !t.artist_id.is_empty() && !t.artist.is_empty();
    let g = ui.painter().layout_no_wrap(t.artist.clone(), font, DIM);
    let pos = egui::pos2(clip.left(), y - g.size().y / 2.0);
    let area = egui::Rect::from_min_size(pos, g.size()).intersect(clip);
    let over = linked && row.hovered() && ui.input(|i| i.pointer.hover_pos()).is_some_and(|p| area.contains(p));
    let color = if over { Color32::WHITE } else { DIM };
    let p = ui.painter().with_clip_rect(clip);
    let g = p.layout_no_wrap(t.artist.clone(), egui::FontId::proportional(size), color);
    let bottom = pos.y + g.size().y;
    let width = g.size().x.min(clip.width());
    p.galley(pos, g, color);
    if over {
        p.line_segment([egui::pos2(pos.x, bottom), egui::pos2(pos.x + width, bottom)], egui::Stroke::new(1.0, color));
    }
    over && row.clicked()
}

/// Chevron button to scroll a shelf (`right = true` points right).
fn arrow_button(ui: &mut egui::Ui, right: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(26.0, 26.0), egui::Sense::click());
    ui.painter().circle_filled(rect.center(), 13.0, if resp.hovered() { Color32::from_gray(60) } else { Color32::from_gray(38) });
    let (c, d) = (rect.center(), if right { 1.0 } else { -1.0 });
    let s = egui::Stroke::new(1.8, Color32::WHITE);
    ui.painter().line_segment([c + egui::vec2(-2.5 * d, -5.0), c + egui::vec2(2.5 * d, 0.0)], s);
    ui.painter().line_segment([c + egui::vec2(2.5 * d, 0.0), c + egui::vec2(-2.5 * d, 5.0)], s);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Context menu opened on a track (list row or queue row).
struct Menu {
    pos: egui::Pos2,
    track: Track,
    /// Index into the queue when opened from the queue panel.
    qidx: Option<usize>,
    frame: u64,
}

enum Act {
    StartRadio(Track),
    PlayNext(Track),
    Enqueue(Track),
    QJump(usize),
    QUp(usize),
    QDown(usize),
    QRemove(usize),
    QMove(usize, usize),
    Like(String, bool),
    QClear,
    AddTo(String, String, String),
    NewWith(Track),
    RemoveFrom(String, String, String),
    /// Open an artist page: (browse id, name).
    GoArtist(String, String),
    /// Open an album page: (browse id, name).
    GoAlbum(String, String),
    /// Open the full list of songs of the artist on screen.
    AllSongs,
}

struct App {
    media: Option<media::Media>,
    /// (playing, paused) last sent to the OS media controls.
    media_state: (bool, bool),
    /// Ids of the user's liked songs.
    liked: HashSet<String>,
    /// Home, Explore and the dynamic page, and which one is on screen (if any).
    feeds: Vec<Feed>,
    page: Option<usize>,
    /// Horizontal scroll offset of each shelf, keyed by (feed, shelf).
    shelf_x: HashMap<(usize, usize), f32>,
    /// Playlists the user owns (editable), the ids already checked, and the id on screen.
    owned: HashSet<String>,
    checked: HashSet<String>,
    sel_id: Option<String>,
    dialog: Option<Dialog>,
    /// Context menu of a sidebar playlist: (position, index, frame it opened).
    pl_menu: Option<(egui::Pos2, usize, u64)>,
    evt_tx: mpsc::Sender<Evt>,
    needs_login: bool,
    logging_in: bool,
    /// New version found by the updater (shown as a banner until dismissed).
    update: Option<update::Release>,
    account: Option<Account>,
    /// Link to the full song list of the artist on screen (its "see all" button).
    more_songs: Option<MoreSongs>,
    /// Search autocomplete: suggestions, the text they were requested for, highlighted row.
    suggest: Vec<String>,
    suggest_req: String,
    suggest_sel: Option<usize>,
    suggest_hover: bool,
    last_query: String,
    typed_at: std::time::Instant,
    checking_update: bool,
    updating: bool,
    cmd: mpsc::Sender<Cmd>,
    audio: mpsc::Sender<Audio>,
    evt: mpsc::Receiver<Evt>,
    state: Arc<Mutex<AudioState>>,
    playlists: Vec<(String, String)>,
    sel_playlist: Option<usize>,
    tracks: Vec<Track>,
    /// Playback queue, independent of the list on screen.
    queue: Vec<Track>,
    qpos: Option<usize>,
    show_queue: bool,
    autoplay: bool,
    radio_pending: bool,
    /// The pending radio was asked for explicitly ("Iniciar radio"), so it applies even with suggestions off.
    radio_force: bool,
    want_next: bool,
    /// A song was requested and is still being downloaded.
    loading: bool,
    /// Queue index being dragged to a new position.
    drag_from: Option<usize>,
    menu: Option<Menu>,
    want: Option<String>,
    playing: bool,
    paused: bool,
    expected_serial: u64,
    /// Video shown full-window (the app goes fullscreen with it); `vfull_app_was_fs` = it already was.
    video_full: bool,
    vfull_app_was_fs: bool,
    vfull_seen_fs: bool,
    /// Debug panel over the fullscreen video, and what it shows (refreshed about once a second).
    vfull_debug: bool,
    /// Lyrics over the fullscreen video (toggled from its context menu).
    vfull_lyrics: bool,
    /// Lyrics settings (Configuración): style, lines in the overlay, and whether fullscreen starts with them on.
    lyr_style: LyricsStyle,
    lyr_lines: u8,
    lyr_default_on: bool,
    /// Highest video height used in fullscreen (480/720/1080; 0 = the screen's, up to 1080).
    fs_quality: u32,
    settings_tab: SettingsTab,
    dbg_at: std::time::Instant,
    dbg_shown: u64,
    dbg_fps: f32,
    dbg_mem: u64,
    dbg_file: f32,
    shuffle: bool,
    /// Queue order before shuffling, to restore it when shuffle is turned off.
    pre_shuffle: Vec<String>,
    rng: u64,
    repeat: Repeat,
    volume: f32,
    quality: Quality,
    prefetch: bool,
    /// Saved default of "show video when the song has one" (Configuración).
    video_default: bool,
    /// Current state of that switch (the Cola can flip it for this session). Off = nothing video-related loads.
    show_video: bool,
    video: Option<video::Video>,
    video_tex: Option<egui::TextureHandle>,
    /// Right panel: the queue or the lyrics of the current song.
    right_tab: RightTab,
    /// Lyrics of the song being shown: (song id, state). Only fetched while the Letra tab is open.
    lyrics: Option<(String, Lyrics)>,
    /// Line of the synced lyrics last scrolled to (so the view only moves when the line changes).
    lyr_line: usize,
    /// Screen scale (physical pixels per point), kept to size the video request.
    ppp: f32,
    /// Files of the current song: normal quality, and the higher one fetched for fullscreen.
    video_sd: Option<PathBuf>,
    video_hd: Option<PathBuf>,
    video_hd_active: bool,
    video_hd_asked: bool,
    show_settings: bool,
    cache_bytes: u64,
    query: String,
    is_search: bool,
    search_q: String,
    pending: Option<(String, Kind)>,
    kind: Kind,
    cards: Vec<Card>,
    view_title: Option<String>,
    history: Vec<Snap>,
    focus_search: bool,
    now: Option<Track>,
    status: String,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        egui_extras::install_image_loaders(&ctx);
        let mut v = egui::Visuals::dark();
        v.panel_fill = BG;
        v.selection.bg_fill = ACCENT;
        ctx.set_visuals(v);
        let (cmd, cmd_rx) = mpsc::channel();
        let (evt_tx, evt) = mpsc::channel();
        let (audio, audio_rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(AudioState::default()));
        spawn_backend(ctx.clone(), cmd_rx, evt_tx.clone());
        spawn_audio(audio_rx, state.clone());
        if session_stale() {
            // Hidden WebView visit: lets Google rotate the tokens, then saves a fresh cookie.
            let (tx, ctx) = (evt_tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                let code = run_child("--refresh").unwrap_or(login::FAILED);
                let _ = tx.send(Evt::Session(code));
                ctx.request_repaint();
            });
        }
        let auto_check = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("installed").exists())).unwrap_or(false)
            && !update::checked_recently();
        std::thread::spawn(|| update::refresh_ytdlp(&ytdlp_path()));
        let _ = cmd.send(Cmd::Account);
        let _ = cmd.send(Cmd::Playlists);
        let _ = audio.send(Audio::Volume(0.7));
        let _ = cmd.send(Cmd::LoadLikes);
        // OS media controls (media keys, volume overlay); needs the native window handle.
        let media = {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            cc.window_handle()
                .ok()
                .and_then(|h| match h.as_raw() {
                    RawWindowHandle::Win32(w) => Some(w.hwnd.get() as *mut std::ffi::c_void),
                    _ => None,
                })
                .and_then(|hwnd| {
                    let (tx, ctx) = (evt_tx.clone(), ctx.clone());
                    media::Media::new(hwnd, move |e| {
                        let _ = tx.send(Evt::Media(e));
                        ctx.request_repaint();
                    })
                })
        };
        let mut app = Self {
            media,
            media_state: (false, false),
            liked: HashSet::new(),
            feeds: vec![
                Feed { title: "Inicio".into(), browse: "FEmusic_home".into(), ..Default::default() },
                Feed { title: "Explorar".into(), browse: "FEmusic_explore".into(), ..Default::default() },
                Feed::default(),
            ],
            page: Some(HOME),
            shelf_x: HashMap::new(),
            owned: HashSet::new(),
            checked: HashSet::new(),
            sel_id: None,
            dialog: None,
            pl_menu: None,
            evt_tx,
            needs_login: false,
            logging_in: false,
            update: None,
            account: None,
            more_songs: None,
            suggest: vec![],
            suggest_req: String::new(),
            suggest_sel: None,
            suggest_hover: false,
            last_query: String::new(),
            typed_at: std::time::Instant::now(),
            checking_update: false,
            updating: false,
            cmd,
            audio,
            evt,
            state,
            playlists: vec![],
            sel_playlist: None,
            tracks: vec![],
            queue: vec![],
            qpos: None,
            show_queue: false,
            autoplay: load_autoplay(),
            radio_pending: false,
            radio_force: false,
            want_next: false,
            loading: false,
            drag_from: None,
            menu: None,
            want: None,
            playing: false,
            paused: false,
            expected_serial: 0,
            video_full: false,
            vfull_app_was_fs: false,
            vfull_seen_fs: false,
            vfull_debug: false,
            vfull_lyrics: load_lyrics_cfg().2,
            lyr_style: load_lyrics_cfg().0,
            lyr_lines: load_lyrics_cfg().1,
            lyr_default_on: load_lyrics_cfg().2,
            fs_quality: load_fs_quality(),
            settings_tab: SettingsTab::Playback,
            dbg_at: std::time::Instant::now(),
            dbg_shown: 0,
            dbg_fps: 0.0,
            dbg_mem: 0,
            dbg_file: 0.0,
            shuffle: false,
            pre_shuffle: Vec::new(),
            rng: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0x9E37_79B9, |d| d.as_nanos() as u64) | 1,
            repeat: Repeat::Off,
            volume: 0.7,
            quality: Quality::load(),
            prefetch: load_prefetch(),
            video_default: load_video(),
            show_video: load_video(),
            video: None,
            video_tex: None,
            right_tab: RightTab::Queue,
            lyrics: None,
            lyr_line: usize::MAX,
            ppp: 1.0,
            video_sd: None,
            video_hd: None,
            video_hd_active: false,
            video_hd_asked: false,
            show_settings: false,
            cache_bytes: 0,
            query: String::new(),
            is_search: false,
            search_q: String::new(),
            pending: None,
            kind: Kind::Songs,
            cards: vec![],
            view_title: None,
            history: vec![],
            focus_search: false,
            now: None,
            status: "Cargando playlists...".into(),
        };
        app.load_feed(HOME);
        if auto_check {
            app.check_update(&ctx, false);
        }
        app
    }

    /// Looks for a newer release on a worker thread; the answer arrives as `Evt::Update`.
    fn check_update(&mut self, ctx: &egui::Context, manual: bool) {
        if self.checking_update || self.updating {
            return;
        }
        self.checking_update = true;
        let (tx, ctx) = (self.evt_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(match update::check() {
                Ok(found) => Evt::Update { found, manual },
                Err(msg) => Evt::UpdateFailed { msg, manual },
            });
            ctx.request_repaint();
        });
    }

    /// Downloads the offered release; `Evt::UpdateReady` follows.
    fn start_update(&mut self, ctx: &egui::Context) {
        let Some(rel) = self.update.clone() else { return };
        if self.updating {
            return;
        }
        self.updating = true;
        self.status = format!("Descargando Temazoo {}...", rel.version);
        let (tx, ctx) = (self.evt_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(match update::download(&rel) {
                Ok(path) => Evt::UpdateReady(path),
                Err(msg) => Evt::UpdateFailed { msg, manual: true },
            });
            ctx.request_repaint();
        });
    }

    /// Requests the first page (or the next one, when a token is stored) of a feed.
    fn load_feed(&mut self, idx: usize) {
        let f = &mut self.feeds[idx];
        if f.loading {
            return;
        }
        f.loading = true;
        let _ = self.cmd.send(Cmd::Feed { idx, browse: f.browse.clone(), params: f.params.clone(), token: f.next.clone() });
    }

    /// Saves the current view so "Volver" can restore it.
    fn push_snap(&mut self) {
        self.history.push(Snap {
            tracks: std::mem::take(&mut self.tracks),
            cards: std::mem::take(&mut self.cards),
            view_title: self.view_title.take(),
            is_search: self.is_search,
            search_q: self.search_q.clone(),
            sel_playlist: self.sel_playlist,
            page: self.page,
            more_songs: self.more_songs.take(),
        });
    }

    /// Opens an artist, album or playlist card.
    fn open_card(&mut self, c: Card) {
        self.push_snap();
        self.page = None;
        self.view_title = Some(c.title.clone());
        self.is_search = false;
        self.sel_playlist = None;
        self.sel_id = None;
        self.query.clear();
        self.status = "Cargando...".into();
        let _ = self.cmd.send(match c.kind {
            CardKind::Artist => Cmd::Artist(c.id),
            CardKind::Album => Cmd::Album(c.id),
            CardKind::Playlist => Cmd::Tracks(c.id),
        });
    }

    /// Opens a mood/genre/section page in the dynamic feed slot.
    fn open_feed(&mut self, title: String, browse: String, params: Option<String>) {
        self.push_snap();
        self.feeds[PAGE] = Feed { title, browse, params, ..Default::default() };
        self.page = Some(PAGE);
        self.sel_playlist = None;
        self.sel_id = None;
        self.query.clear();
        self.load_feed(PAGE);
    }

    /// Opens the login window in a child process; the result arrives as `Evt::Session`.
    fn start_login(&mut self, ctx: &egui::Context) {
        if self.logging_in {
            return;
        }
        self.logging_in = true;
        self.status = "Esperando el inicio de sesión...".into();
        let (tx, ctx) = (self.evt_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let code = run_child("--login").unwrap_or(login::FAILED);
            let _ = tx.send(Evt::Session(code));
            ctx.request_repaint();
        });
    }

    /// Starts the song picked in the visible list: the queue becomes that list.
    fn play_index(&mut self, i: usize) {
        let q = self.tracks.clone();
        self.start_queue(q, i);
    }

    /// Replaces the queue and plays `pos`. With shuffle on, that song goes first and the rest is mixed.
    fn start_queue(&mut self, q: Vec<Track>, pos: usize) {
        self.queue = q;
        self.pre_shuffle.clear();
        let mut pos = pos;
        if self.shuffle && !self.queue.is_empty() {
            self.pre_shuffle = self.queue.iter().map(|t| t.id.clone()).collect();
            let first = self.queue.remove(pos.min(self.queue.len() - 1));
            self.shuffle_slice(0);
            self.queue.insert(0, first);
            pos = 0;
        }
        self.play_queue(pos);
    }

    fn next_rand(&mut self) -> u64 {
        // xorshift64: good enough to mix a playlist, no extra dependency.
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// Fisher-Yates over `queue[from..]`.
    fn shuffle_slice(&mut self, from: usize) {
        let n = self.queue.len();
        for i in (from + 1..n).rev() {
            let j = from + (self.next_rand() % (i - from + 1) as u64) as usize;
            self.queue.swap(i, j);
        }
    }

    /// Turns shuffle on (mixes what is left in the queue) or off (restores the original order of it).
    fn set_shuffle(&mut self, on: bool) {
        self.shuffle = on;
        let from = self.qpos.map_or(0, |p| p + 1).min(self.queue.len());
        if on {
            self.pre_shuffle = self.queue.iter().map(|t| t.id.clone()).collect();
            self.shuffle_slice(from);
        } else {
            let order = std::mem::take(&mut self.pre_shuffle);
            let idx = |t: &Track| order.iter().position(|id| *id == t.id).unwrap_or(usize::MAX);
            self.queue[from..].sort_by_key(idx);
        }
    }

    fn play_queue(&mut self, pos: usize) {
        let Some(t) = self.queue.get(pos).cloned() else { return };
        self.qpos = Some(pos);
        if let Some(m) = &mut self.media {
            m.set_track(&t.title, &t.artist, &t.cover, t.secs);
        }
        self.now = Some(t.clone());
        self.want = Some(t.id.clone());
        // Video: drop the previous one right away; look up the new one only when the setting is on.
        self.video_reset();
        if self.show_video {
            let _ = self.cmd.send(Cmd::Video(t.id.clone(), self.panel_video_height()));
        }
        self.playing = false;
        self.paused = false;
        self.want_next = false;
        self.loading = true;
        self.status = "Descargando...".into();
        // Stop the previous song right away so the click visibly does something.
        let _ = self.audio.send(Audio::Fade);
        let _ = self.cmd.send(Cmd::Fetch(t.clone(), false, self.quality));
        self.ensure_suggestions();
    }

    /// Writes every setting to `config.json`.
    fn save_settings(&self) {
        save_config(self.quality, self.prefetch, self.autoplay, self.video_default, (self.lyr_style, self.lyr_lines, self.lyr_default_on), self.fs_quality);
    }

    /// Turns the video on or off for now. Off drops the decoder thread and texture right away.
    /// Lines of the fullscreen debug panel. `screen` is the video area in physical pixels.
    /// Memory and file size are refreshed about once a second (they are not free to query).
    fn debug_lines(&mut self, screen: egui::Vec2) -> Vec<String> {
        let Some(v) = &self.video else {
            // No video for this song: the cover is shown instead.
            let pos = self.state.lock().unwrap().pos.as_secs();
            return vec![
                "Video      ninguno para esta canción (se muestra la carátula)".to_string(),
                format!("Pantalla   {}x{} px", screen.x as u32, screen.y as u32),
                format!("Audio      {}  posición {}:{:02}", if self.quality == Quality::Opus { "Opus" } else { "AAC" }, pos / 60, pos % 60),
                format!("Memoria    {} MB (toda la app)", process_mem_mb().unwrap_or(0)),
            ];
        };
        let st = v.stats();
        let secs = self.dbg_at.elapsed().as_secs_f32();
        if secs >= 1.0 {
            self.dbg_fps = st.shown.saturating_sub(self.dbg_shown) as f32 / secs;
            self.dbg_shown = st.shown;
            self.dbg_at = std::time::Instant::now();
            self.dbg_mem = process_mem_mb().unwrap_or(0);
            let file = if self.video_hd_active { &self.video_hd } else { &self.video_sd };
            self.dbg_file = file.as_ref().and_then(|p| std::fs::metadata(p).ok()).map_or(0.0, |m| m.len() as f32 / 1_048_576.0);
        }
        let (tw, th) = self.video_tex.as_ref().map_or((0.0, 0.0), |t| (t.size()[0] as f32, t.size()[1] as f32));
        let scale = if tw > 0.0 && th > 0.0 { (screen.x / tw).min(screen.y / th) } else { 0.0 };
        let pos = self.state.lock().unwrap().pos.as_secs();
        vec![
            format!("Video      {}x{}  H.264  {}", tw as u32, th as u32, if self.video_hd_active { "alta resolución" } else { "resolución normal" }),
            format!("Archivo    {:.1} MB{}", self.dbg_file, if self.video_hd_asked && !self.video_hd_active && self.video_hd.is_none() { "  (buscando mejor versión...)" } else { "" }),
            format!("Pantalla   {}x{} px  (video a x{:.2})", screen.x as u32, screen.y as u32, scale),
            format!("Cuadros    {:.1}/s  mostrados {}  descartados {}", self.dbg_fps, st.shown, st.dropped),
            format!("Decodifica {:.1} ms/cuadro  color {:.1} ms", st.decode_ms, st.convert_ms),
            format!("Sincronía  {:+} ms respecto al audio", st.sync_ms),
            format!("Audio      {}  posición {}:{:02}", if self.quality == Quality::Opus { "Opus" } else { "AAC" }, pos / 60, pos % 60),
            format!("Memoria    {} MB (toda la app)", self.dbg_mem),
        ]
    }

    /// The lyrics of the current song: synced (highlighting the line that is playing, click to jump) or plain.
    /// Used in the queue panel and, with `big`, over the fullscreen video.
    fn lyrics_ui(&self, ui: &mut egui::Ui, now_ms: u32, big: bool, seek_to: &mut Option<u32>, new_line: &mut usize) {
        let (size, size_cur, size_plain, note) = if big { (24.0, 30.0, 22.0, 14.0) } else { (16.0, 18.0, 15.0, 11.5) };
        let salt = if big { "lyrics_full" } else { "lyrics_panel" };
        match (&self.now, &self.lyrics) {
            (None, _) => {
                ui.label(egui::RichText::new("Poné una canción para ver su letra.").color(DIM).size(13.0));
            }
            (_, Some((_, Lyrics::Synced { lines, source }))) => {
                // The current line is the last one whose time has passed.
                let cur = lines.partition_point(|(t, _)| *t <= now_ms).checked_sub(1);
                let scroll_now = cur.is_some_and(|c| c != self.lyr_line) || self.lyr_line == usize::MAX;
                egui::ScrollArea::vertical().id_salt(salt).auto_shrink([false, false]).show(ui, |ui| {
                    ui.add_space(8.0);
                    for (i, (t, text)) in lines.iter().enumerate() {
                        let is_cur = Some(i) == cur;
                        let col = if is_cur {
                            Color32::WHITE
                        } else if cur.is_some_and(|c| i < c) {
                            Color32::from_gray(105)
                        } else {
                            Color32::from_gray(165)
                        };
                        let label = if text.is_empty() { "· · ·".to_string() } else { text.clone() };
                        let mut rt = egui::RichText::new(label).size(if is_cur { size_cur } else { size }).color(col);
                        if is_cur {
                            rt = rt.strong();
                        }
                        let r = ui.add(egui::Label::new(rt).wrap().sense(egui::Sense::click()));
                        if r.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        // Click a line to jump to it.
                        if r.clicked() {
                            *seek_to = Some(*t);
                        }
                        if is_cur && scroll_now {
                            r.scroll_to_me(Some(egui::Align::Center));
                            *new_line = i;
                        }
                        ui.add_space(if big { 12.0 } else { 8.0 });
                    }
                    ui.add_space(14.0);
                    ui.label(egui::RichText::new(format!("Fuente: {source}  ·  clic en una línea para saltar")).color(DIM).size(note));
                    ui.add_space(if big { 300.0 } else { 120.0 });
                });
                // Keep the highlight moving between the app's normal redraws.
                ui.ctx().request_repaint_after(Duration::from_millis(120));
            }
            (_, Some((_, Lyrics::Plain { text, source }))) => {
                egui::ScrollArea::vertical().id_salt(salt).auto_shrink([false, false]).show(ui, |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(text).size(size_plain).color(Color32::from_gray(225))).wrap());
                    if !source.is_empty() {
                        ui.add_space(14.0);
                        ui.label(egui::RichText::new(format!("Fuente: {source}")).color(DIM).size(note));
                    }
                });
            }
            (_, Some((_, Lyrics::Missing))) => {
                ui.label(egui::RichText::new("No hay letra para esta canción.").color(DIM).size(13.0));
            }
            _ => {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(14.0).color(ACCENT));
                    ui.label(egui::RichText::new("Buscando letra...").color(DIM).size(13.0));
                });
            }
        }
    }

    /// Synced lyrics over the picture: a few lines at the bottom, the current one bright and the rest fading
    /// with distance. When the line changes they glide up instead of jumping (a short eased transition).
    fn lyrics_overlay(&self, ctx: &egui::Context, painter: &egui::Painter, media: egui::Rect, now_ms: u32) {
        let Some((_, Lyrics::Synced { lines, .. })) = &self.lyrics else { return };
        let cur = lines.partition_point(|(t, _)| *t <= now_ms).checked_sub(1);
        // Fractional index of the line in the "focus": it moves from the previous line to the current one
        // over 350 ms after the current one starts.
        let since = cur.map_or(u32::MAX, |c| now_ms.saturating_sub(lines[c].0));
        let t = (since as f32 / 350.0).min(1.0);
        let ease = t * t * (3.0 - 2.0 * t);
        let focus = cur.map_or(-1.0, |c| c as f32 - 1.0 + ease);
        ctx.request_repaint_after(Duration::from_millis(if since < 400 { 16 } else { 120 }));

        let half = (self.lyr_lines / 2) as f32;
        let size = (media.height() * 0.05).clamp(20.0, 64.0);
        let slot = size * 1.75;
        let center_y = media.bottom() - media.height() * 0.13 - half * slot;
        // A soft dark band behind the text so it reads over any picture.
        let band_h = (half * 2.0 + 2.5) * slot;
        const STRIPS: usize = 14;
        for k in 0..STRIPS {
            let y0 = media.bottom() - band_h + band_h * k as f32 / STRIPS as f32;
            let a = (150.0 * (k + 1) as f32 / STRIPS as f32) as u8;
            painter.rect_filled(
                egui::Rect::from_min_max(egui::pos2(media.left(), y0), egui::pos2(media.right(), y0 + band_h / STRIPS as f32 + 1.0)),
                0.0,
                Color32::from_black_alpha(a),
            );
        }
        let first = (focus - half - 1.0).floor().max(0.0) as usize;
        let last = ((focus + half + 1.0).ceil().max(0.0) as usize).min(lines.len().saturating_sub(1));
        for i in first..=last {
            let text = &lines[i].1;
            if text.is_empty() {
                continue;
            }
            let d = i as f32 - focus;
            // Opaque at the focus, fading to nothing just beyond the visible lines.
            let a = (1.0 - d.abs() / (half + 1.0)).clamp(0.0, 1.0).powf(1.5);
            if a < 0.02 {
                continue;
            }
            let sz = size * (1.0 - 0.18 * d.abs().min(1.0));
            let wrap = media.width() * 0.8;
            let fg = painter.layout(text.clone(), egui::FontId::proportional(sz), Color32::from_white_alpha((255.0 * a) as u8), wrap);
            let shadow = painter.layout(text.clone(), egui::FontId::proportional(sz), Color32::from_black_alpha((200.0 * a) as u8), wrap);
            let pos = egui::pos2(media.center().x - fg.size().x / 2.0, center_y + d * slot - fg.size().y / 2.0);
            painter.galley(pos + egui::vec2(2.0, 2.0), shadow, Color32::BLACK);
            painter.galley(pos, fg, Color32::WHITE);
        }
    }

    /// Tallest video worth fetching for the queue panel: its width in real pixels at 16:9, rounded up to
    /// a size YouTube serves. Anything bigger would only cost memory and bandwidth.
    fn panel_video_height(&self) -> u32 {
        let px_w = 306.0 * self.ppp;
        let h = px_w * 9.0 / 16.0;
        if h <= 240.0 {
            240
        } else if h <= 360.0 {
            360
        } else {
            480
        }
    }

    /// Drops the decoder, its texture and everything remembered about the current song's video.
    fn video_reset(&mut self) {
        self.video = None;
        self.video_tex = None;
        self.video_sd = None;
        self.video_hd = None;
        self.video_hd_active = false;
        self.video_hd_asked = false;
    }

    /// (Re)starts the decoder on `path`, jumping to where the song is now.
    fn start_video_file(&mut self, ctx: &egui::Context, path: PathBuf) {
        let (pos, at) = {
            let s = self.state.lock().unwrap();
            (s.pos, s.at)
        };
        let running = self.playing && !self.paused;
        // The audio position is a sample taken a moment ago: add the time since.
        let pos = if running { pos + at.map_or(Duration::ZERO, |t| t.elapsed()) } else { pos };
        let c = ctx.clone();
        self.video = Some(video::Video::start(path, pos, running, move || c.request_repaint()));
    }

    /// Switches between the normal and the high-resolution file (the latter is used in fullscreen).
    fn use_video_hd(&mut self, ctx: &egui::Context, hd: bool) {
        let path = if hd { self.video_hd.clone() } else { self.video_sd.clone() };
        if let Some(p) = path.filter(|_| self.video.is_some() && self.video_hd_active != hd) {
            self.start_video_file(ctx, p);
            self.video_hd_active = hd;
        }
    }

    fn set_video(&mut self, on: bool) {
        self.show_video = on;
        if on {
            if let Some(id) = self.want.clone().filter(|_| self.now.is_some()) {
                let _ = self.cmd.send(Cmd::Video(id, self.panel_video_height()));
            }
        } else {
            self.video_reset();
        }
    }

    fn toggle_pause(&mut self) {
        if self.playing {
            self.paused = !self.paused;
            let _ = self.audio.send(if self.paused { Audio::Pause } else { Audio::Resume });
        }
    }

    /// Returns false when there is nothing after the current song.
    fn step(&mut self, delta: isize) -> bool {
        let Some(p) = self.qpos else { return false };
        let j = p as isize + delta;
        if (0..self.queue.len() as isize).contains(&j) {
            self.play_queue(j as usize);
            true
        } else if self.repeat == Repeat::All && !self.queue.is_empty() {
            // Repeat all: wrap around.
            self.play_queue(if delta > 0 { 0 } else { self.queue.len() - 1 });
            true
        } else {
            false
        }
    }

    /// With suggestions on, when the playing song is the last one, fetch its radio.
    fn ensure_suggestions(&mut self) {
        let last = self.qpos.is_some_and(|p| p + 1 >= self.queue.len());
        if self.autoplay && last && !self.radio_pending {
            if let Some(t) = &self.now {
                self.radio_pending = true;
                let _ = self.cmd.send(Cmd::Radio(t.id.clone()));
            }
        }
    }

    fn enqueue(&mut self, t: Track, next: bool) {
        if self.now.is_none() {
            self.queue = vec![t];
            self.play_queue(0);
            return;
        }
        let at = if next { self.qpos.map_or(0, |p| p + 1).min(self.queue.len()) } else { self.queue.len() };
        self.queue.insert(at, t);
        self.status = if next { "Se reproducirá a continuación".into() } else { "Agregada a la cola".into() };
    }

    fn act(&mut self, a: Act) {
        match a {
            Act::StartRadio(t) => {
                // Replace the queue with this song and let its radio fill the rest.
                let id = t.id.clone();
                self.queue = vec![t];
                self.radio_pending = true;
                self.radio_force = true;
                self.play_queue(0);
                let _ = self.cmd.send(Cmd::Radio(id));
                self.status = "Iniciando radio...".into();
            }
            Act::PlayNext(t) => self.enqueue(t, true),
            Act::Enqueue(t) => self.enqueue(t, false),
            Act::QJump(i) => self.play_queue(i),
            Act::QUp(i) if i > 0 => self.queue.swap(i, i - 1),
            Act::QDown(i) if i + 1 < self.queue.len() => self.queue.swap(i, i + 1),
            Act::QRemove(i) if i < self.queue.len() => {
                self.queue.remove(i);
            }
            Act::QMove(from, to) if from < self.queue.len() && to < self.queue.len() => {
                let t = self.queue.remove(from);
                self.queue.insert(to, t);
            }
            Act::Like(id, like) => {
                // Optimistic: update the heart now, revert if the server refuses.
                if like {
                    self.liked.insert(id.clone());
                } else {
                    self.liked.remove(&id);
                }
                let _ = self.cmd.send(Cmd::Rate { vid: id, like });
            }
            Act::QClear => match self.qpos {
                Some(p) => self.queue.truncate(p + 1),
                None => self.queue.clear(),
            },
            Act::AddTo(id, name, vid) => {
                let _ = self.cmd.send(Cmd::AddTo { id, name, vid });
            }
            Act::RemoveFrom(id, vid, set) => {
                let _ = self.cmd.send(Cmd::RemoveFrom { id, vid, set });
            }
            Act::NewWith(t) => {
                self.dialog = Some(Dialog { kind: DlgKind::New { with: Some(t) }, name: String::new(), desc: String::new(), privacy: Priv::Private });
            }
            Act::GoArtist(id, name) => {
                self.open_card(Card { kind: CardKind::Artist, id, title: name, subtitle: String::new(), thumb: String::new(), section: String::new() });
            }
            Act::GoAlbum(id, name) => {
                let title = if name.is_empty() { "Álbum".to_string() } else { name };
                self.open_card(Card { kind: CardKind::Album, id, title, subtitle: String::new(), thumb: String::new(), section: String::new() });
            }
            Act::AllSongs => {
                if let Some((browse, params)) = self.more_songs.clone() {
                    let title = format!("{} · Canciones", self.view_title.clone().unwrap_or_default());
                    self.push_snap();
                    self.view_title = Some(title);
                    self.is_search = false;
                    self.sel_playlist = None;
                    self.sel_id = None;
                    self.query.clear();
                    self.status = "Cargando...".into();
                    let _ = self.cmd.send(Cmd::MoreSongs(browse, params));
                }
            }
            _ => {}
        }
        // Edits may have left the playing song as the last one.
        self.ensure_suggestions();
    }
}

fn parse_dur(s: &str) -> Option<u32> {
    s.split(':').try_fold(0u32, |a, p| Some(a * 60 + p.trim().parse::<u32>().ok()?))
}

fn fmt(secs: u32) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(e) = self.evt.try_recv() {
            match e {
                Evt::Session(code) => {
                    self.logging_in = false;
                    match code {
                        login::OK => {
                            self.status = "Sesión actualizada".into();
                            let _ = self.cmd.send(Cmd::Reauth);
                            let _ = self.cmd.send(Cmd::Account);
                            let _ = self.cmd.send(Cmd::Playlists);
                            let _ = self.cmd.send(Cmd::LoadLikes);
                            // The pages were fetched with the old session: reload the one on screen.
                            for f in &mut self.feeds {
                                f.shelves.clear();
                                f.next = None;
                                f.loaded = false;
                                f.loading = false;
                            }
                            if let Some(p) = self.page.filter(|p| *p != PAGE) {
                                self.load_feed(p);
                            }
                        }
                        login::NEEDS_LOGIN => {
                            self.needs_login = true;
                            self.status = "Hace falta iniciar sesión".into();
                        }
                        login::CLOSED => self.status = "Inicio de sesión cancelado".into(),
                        _ => self.status = "No se pudo completar el inicio de sesión".into(),
                    }
                }
                Evt::Feed { idx, shelves, next, first } => {
                    let f = &mut self.feeds[idx];
                    if first {
                        f.shelves = shelves;
                    } else {
                        f.shelves.extend(shelves);
                    }
                    f.next = next;
                    f.loading = false;
                    f.loaded = true;
                }
                Evt::Media(e) => {
                    use souvlaki::MediaControlEvent as M;
                    match e {
                        M::Toggle => self.toggle_pause(),
                        M::Play if self.paused => self.toggle_pause(),
                        M::Pause | M::Stop if !self.paused => self.toggle_pause(),
                        M::Next => {
                            self.step(1);
                        }
                        M::Previous => {
                            self.step(-1);
                        }
                        _ => {}
                    }
                }
                Evt::Likes(ids) => self.liked.extend(ids),
                Evt::RateFailed(vid, like, err) => {
                    if like {
                        self.liked.remove(&vid);
                    } else {
                        self.liked.insert(vid);
                    }
                    self.status = format!("No se pudo actualizar Me gusta: {err}");
                }
                Evt::Owned(owned, checked) => {
                    self.owned.extend(owned);
                    self.checked.extend(checked);
                }
                Evt::EditInfo(id, name, desc, privacy) => {
                    self.dialog = Some(Dialog { kind: DlgKind::Edit { id }, name, desc, privacy });
                }
                Evt::Created(id) => {
                    self.owned.insert(id.clone());
                    self.checked.insert(id);
                    self.status = "Playlist creada".into();
                    let _ = self.cmd.send(Cmd::Playlists);
                }
                Evt::Done { msg, reload } => {
                    self.status = msg;
                    if reload {
                        let _ = self.cmd.send(Cmd::Playlists);
                    }
                }
                Evt::Removed(set) => {
                    self.tracks.retain(|t| t.svid != set);
                    self.status = "Canción quitada de la playlist".into();
                }
                Evt::Playlists(p) => {
                    self.needs_login = p.is_empty();
                    self.status = if p.is_empty() {
                        "No se encontraron playlists: hace falta iniciar sesión.".into()
                    } else {
                        format!("{} playlists", p.len())
                    };
                    self.playlists = p;
                    self.sel_playlist = self.sel_id.as_ref().and_then(|id| self.playlists.iter().position(|x| &x.0 == id));
                    // User-made playlists start with "PL"; ask which of them are really ours.
                    let todo: Vec<String> = self
                        .playlists
                        .iter()
                        .map(|x| x.0.clone())
                        .filter(|id| id.starts_with("PL") && !self.checked.contains(id))
                        .collect();
                    if !todo.is_empty() {
                        let _ = self.cmd.send(Cmd::CheckOwned(todo));
                    }
                }
                Evt::Results(q, kind, t, c) => {
                    if self.pending.as_ref() == Some(&(q.clone(), kind)) {
                        self.status = format!("{} resultados", t.len() + c.len());
                        self.tracks = t;
                        self.cards = c;
                        self.is_search = true;
                        self.more_songs = None;
                        self.page = None;
                        self.sel_playlist = None;
                        self.sel_id = None;
                        self.view_title = None;
                        self.search_q = q;
                        self.query.clear();
                    }
                }
                Evt::Artist(name, t, c, more) => {
                    self.status = format!("{} canciones, {} más", t.len(), c.len());
                    self.tracks = t;
                    self.cards = c;
                    self.is_search = false;
                    self.view_title = Some(name);
                    self.more_songs = more;
                }
                Evt::Tracks(t) => {
                    self.cards.clear();
                    self.more_songs = None;
                    self.status = format!("{} canciones", t.len());
                    self.tracks = t;
                }
                Evt::Radio(mut t) => {
                    self.radio_pending = false;
                    let forced = std::mem::take(&mut self.radio_force);
                    t.retain(|s| !self.queue.iter().any(|q| q.id == s.id));
                    if (self.autoplay || forced) && !t.is_empty() {
                        self.queue.extend(t);
                        if self.want_next {
                            self.step(1);
                        }
                    }
                }
                Evt::Ready(id, path) => {
                    if self.want.as_deref() == Some(id.as_str()) {
                        let _ = self.audio.send(Audio::Load(path));
                        self.expected_serial += 1;
                        self.playing = true;
                        self.loading = false;
                        self.status = "Reproduciendo".into();
                        if let Some(n) = self.qpos.and_then(|i| self.queue.get(i + 1)).cloned().filter(|_| self.prefetch) {
                            let _ = self.cmd.send(Cmd::Fetch(n, true, self.quality));
                        }
                    }
                }
                Evt::Video(id, Some(path)) => {
                    if self.show_video && self.want.as_deref() == Some(id.as_str()) {
                        self.video_sd = Some(path.clone());
                        self.start_video_file(&ctx, path);
                        // The video is shown inside the queue panel, so open it.
                        self.show_queue = true;
                    }
                }
                Evt::Video(..) => {}
                Evt::VideoHd(id, Some(path)) => {
                    if self.show_video && self.want.as_deref() == Some(id.as_str()) {
                        self.video_hd = Some(path);
                        if self.video_full {
                            self.use_video_hd(&ctx, true);
                        }
                    }
                }
                Evt::VideoHd(..) => {}
                Evt::Lyrics(id, found) => {
                    if self.lyrics.as_ref().is_some_and(|(cur, _)| *cur == id) {
                        self.lyrics = Some((id, found.unwrap_or(Lyrics::Missing)));
                        self.lyr_line = usize::MAX;
                    }
                }
                Evt::Prefetched => {}
                Evt::Account(a) => self.account = a,
                Evt::Suggest(input, list) => {
                    // Ignore late answers for text that is no longer in the box.
                    if input == self.query.trim() {
                        self.suggest = list;
                        self.suggest_sel = None;
                    }
                }
                Evt::Update { found, manual } => {
                    self.checking_update = false;
                    if found.is_none() && manual {
                        self.status = format!("Temazoo {} está al día", update::VERSION);
                    }
                    if found.is_some() {
                        self.update = found;
                    }
                }
                Evt::UpdateFailed { msg, manual } => {
                    self.checking_update = false;
                    self.updating = false;
                    if manual {
                        self.status = format!("Actualización: {msg}");
                    }
                }
                Evt::UpdateReady(path) => match update::apply(&path) {
                    Ok(()) => {
                        kill_children();
                        std::process::exit(0)
                    }
                    Err(m) => {
                        self.updating = false;
                        self.status = m;
                    }
                },
                Evt::Error(m) => {
                    self.loading = false;
                    for f in &mut self.feeds {
                        f.loading = false;
                    }
                    self.status = m;
                }
            }
        }
        let finished = {
            let s = self.state.lock().unwrap();
            s.finished && s.serial == self.expected_serial
        };
        if self.playing && finished {
            self.playing = false;
            // Repeat one: play the same song again.
            if self.repeat == Repeat::One {
                if let Some(p) = self.qpos {
                    self.play_queue(p);
                    ctx.request_repaint();
                    return;
                }
            }
            // Nothing left: wait for suggestions if they are on (and ask for them if not yet requested).
            if !self.step(1) && self.autoplay {
                self.want_next = true;
                self.ensure_suggestions();
            }
        }
        // `ui` doesn't run while the window is minimized, but `logic` does: keep it ticking so the
        // next song starts on its own (and media keys keep working) in the background.
        if self.playing || self.loading || self.want_next {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.playing || self.loading {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        // Rows that react to clicks set this; the hand cursor is applied last so nothing overrides it.
        let mut hand = false;
        let (pos, pos_at, total_audio) = {
            let s = self.state.lock().unwrap();
            (s.pos, s.at, s.total.as_secs() as u32)
        };
        let cur = self.now.clone();
        // Keyboard: Space pauses/resumes (unless typing); Ctrl+Left/Right = previous/next.
        if !ctx.egui_wants_keyboard_input() && ctx.input(|i| i.key_pressed(egui::Key::Space)) {
            self.toggle_pause();
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::ArrowRight)) {
            self.step(1);
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::ArrowLeft)) {
            self.step(-1);
        }
        let media_now = (self.playing || self.loading, self.paused);
        if media_now != self.media_state {
            self.media_state = media_now;
            if let Some(m) = &mut self.media {
                m.set_state(media_now.0, media_now.1);
            }
        }
        let mut acts: Vec<Act> = Vec::new();
        let mut open_menu: Option<Menu> = None;
        let mut toggle_autoplay = false;
        let mut start_login = false;
        let mut drag_start: Option<usize> = None;
        let mut drop_to: Option<usize> = None;
        let mut feed_click: Option<(usize, usize, usize)> = None;
        let mut more_feed: Option<usize> = None;
        let mut nav_feed: Option<usize> = None;
        let frame_nr = ctx.cumulative_frame_nr();
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F)) {
            self.focus_search = true;
        }
        // Fullscreen: F11 toggles the whole app; Esc leaves it (or the fullscreen video first).
        let app_fs = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        let mut toggle_app_fs = ctx.input(|i| i.key_pressed(egui::Key::F11));
        if app_fs && !self.video_full && !ctx.egui_wants_keyboard_input() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            toggle_app_fs = true;
        }
        let mut enter_vfull = false;
        // Leaves on Esc, or when there is nothing playing or the video setting was turned off.
        let mut exit_vfull = self.video_full && (self.now.is_none() || !self.show_video || ctx.input(|i| i.key_pressed(egui::Key::Escape)));
        // Leaving the app's fullscreen (F11, the bar button, the window) also puts the video back in its place.
        // `vfull_seen_fs` avoids reacting before the window has actually gone fullscreen.
        if self.video_full {
            if app_fs {
                self.vfull_seen_fs = true;
            } else if self.vfull_seen_fs {
                exit_vfull = true;
            }
        }

        // ---- video (only exists while the setting is on and the song has one) ----
        // The video lives at the top of the queue panel (or fills the window in fullscreen). While it is
        // not on screen the decoder, its buffers and the texture are released entirely; they come back
        // (jumping to the current position) as soon as it is visible again.
        self.ppp = ctx.pixels_per_point();
        let video_visible = self.show_queue || self.video_full;
        if self.show_video && !video_visible && self.video.is_some() {
            self.video = None;
            self.video_tex = None;
            self.video_hd_active = false;
        } else if self.show_video && video_visible && self.video.is_none() {
            let hd = self.video_full && self.video_hd.is_some();
            if let Some(p) = if hd { self.video_hd.clone() } else { self.video_sd.clone() } {
                self.start_video_file(&ctx, p);
                self.video_hd_active = hd;
            }
        }
        // Fullscreen wants a higher resolution than the panel. Checked every frame (not just when entering):
        // the small video may arrive after fullscreen was entered, and the big one after that.
        if self.video_full && self.show_video {
            if let Some(have) = self.video.as_ref().map(|v| v.height()).filter(|h| *h > 0) {
                if self.video_hd.is_some() {
                    self.use_video_hd(&ctx, true);
                } else if !self.video_hd_asked {
                    if let Some(id) = self.want.clone() {
                        self.video_hd_asked = true;
                        // What the screen can show, limited by the quality chosen in Configuración.
                        let screen_h = ((ctx.content_rect().height() - 92.0) * ctx.pixels_per_point()) as u32;
                        let cap = if self.fs_quality == 0 { 1080 } else { self.fs_quality };
                        let _ = self.cmd.send(Cmd::VideoHd(id, screen_h.min(cap).max(240), have));
                    }
                }
            }
        }
        if let Some(v) = self.video.as_ref().filter(|_| video_visible) {
            v.set_position(pos, pos_at, self.playing && !self.paused);
            if let Some(f) = v.take_frame() {
                // The picture is opaque, so premultiplied = straight and skips a per-pixel pass.
                let img = egui::ColorImage::from_rgba_premultiplied([f.w, f.h], &f.rgba);
                match &mut self.video_tex {
                    Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                    None => self.video_tex = Some(ctx.load_texture("video", img, egui::TextureOptions::LINEAR)),
                }
            }
        }
        let mut toggle_video = false;
        let mut toggle_shuffle = false;

        // ---- player bar ----
        egui::Panel::bottom("player")
            .exact_size(92.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(BAR).inner_margin(12.0))
            .show(ui, |ui| {
                use egui_extras::{Size, StripBuilder};
                StripBuilder::new(ui)
                    .size(Size::exact(280.0))
                    .size(Size::remainder())
                    .size(Size::exact(190.0))
                    .horizontal(|mut strip| {
                        strip.cell(|ui| {
                            ui.horizontal(|ui| {
                                let (rect, _) = ui.allocate_exact_size(egui::vec2(66.0, 66.0), egui::Sense::hover());
                                if let Some(t) = &cur {
                                    cover(ui, &t.cover, rect, 8.0);
                                }
                                ui.vertical(|ui| {
                                    ui.add_space(10.0);
                                    match &cur {
                                        Some(t) => {
                                            ui.add(egui::Label::new(egui::RichText::new(&t.title).strong().size(15.0)).truncate());
                                            ui.add(egui::Label::new(egui::RichText::new(&t.artist).color(DIM)).truncate());
                                        }
                                        None => {
                                            ui.label(egui::RichText::new("Nada en reproducción").color(DIM));
                                        }
                                    }
                                });
                            });
                        });
                        strip.cell(|ui| {
                            let avail = ui.available_width();
                            ui.horizontal(|ui| {
                                // Fixed row height so every button is centred on the same line, whatever its size.
                                ui.set_height(40.0);
                                ui.add_space(((avail - 260.0) / 2.0).max(0.0));
                                ui.spacing_mut().item_spacing.x = 16.0;
                                if shuffle_button(ui, self.shuffle).on_hover_text(if self.shuffle { "Aleatorio: activado" } else { "Aleatorio" }).clicked() {
                                    toggle_shuffle = true;
                                }
                                if icon_button(ui, Icon::Prev, 32.0, false).clicked() {
                                    self.step(-1);
                                }
                                let icon = if self.playing && !self.paused { Icon::Pause } else { Icon::Play };
                                if icon_button(ui, icon, 40.0, true).clicked() && self.playing {
                                    self.paused = !self.paused;
                                    let _ = self.audio.send(if self.paused { Audio::Pause } else { Audio::Resume });
                                }
                                if icon_button(ui, Icon::Next, 32.0, false).clicked() {
                                    self.step(1);
                                }
                                let tip = match self.repeat {
                                    Repeat::Off => "Repetir: no",
                                    Repeat::All => "Repetir: toda la cola",
                                    Repeat::One => "Repetir: esta canción",
                                };
                                if repeat_button(ui, self.repeat).on_hover_text(tip).clicked() {
                                    self.repeat = match self.repeat {
                                        Repeat::Off => Repeat::All,
                                        Repeat::All => Repeat::One,
                                        Repeat::One => Repeat::Off,
                                    };
                                }
                                if let Some(t) = &cur {
                                    let liked = self.liked.contains(&t.id);
                                    if heart_button(ui, liked).on_hover_text(if liked { "Quitar de Me gusta" } else { "Me gusta" }).clicked() {
                                        acts.push(Act::Like(t.id.clone(), !liked));
                                    }
                                }
                            });
                            if self.loading {
                                ui.horizontal(|ui| {
                                    ui.add_space(((avail - 110.0) / 2.0).max(0.0));
                                    ui.add(egui::Spinner::new().size(14.0).color(ACCENT));
                                    ui.label(egui::RichText::new("Cargando...").color(DIM).size(12.0));
                                });
                            } else if let Some(t) = &cur {
                                let secs = if total_audio > 0 { total_audio } else { t.secs };
                                let frac = if secs > 0 { (pos.as_secs_f32() / secs as f32).clamp(0.0, 1.0) } else { 0.0 };
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 8.0;
                                    ui.label(egui::RichText::new(fmt(pos.as_secs() as u32)).color(DIM).size(12.0));
                                    let w = (ui.available_width() - 50.0).max(40.0);
                                    let (r, f) = thin_bar(ui, w, frac);
                                    if (r.drag_stopped() || r.clicked()) && secs > 0 {
                                        let _ = self.audio.send(Audio::Seek(Duration::from_secs_f32(f * secs as f32)));
                                    }
                                    ui.label(egui::RichText::new(fmt(secs)).color(DIM).size(12.0));
                                });
                            }
                        });
                        strip.cell(|ui| {
                            ui.add_space(14.0);
                            ui.horizontal(|ui| {
                                if queue_button(ui, self.show_queue).on_hover_text("Cola").clicked() {
                                    self.show_queue = !self.show_queue;
                                }
                                if fs_button(ui, app_fs).on_hover_text(if app_fs { "Salir de pantalla completa (F11)" } else { "Pantalla completa (F11)" }).clicked() {
                                    toggle_app_fs = true;
                                }
                                ui.label(egui::RichText::new("Vol").color(DIM).size(12.0));
                                let (r, f) = thin_bar(ui, 64.0, self.volume);
                                if r.dragged() || r.clicked() {
                                    self.volume = f;
                                    let _ = self.audio.send(Audio::Volume(f));
                                }
                            });
                        });
                    });
            });

        // ---- sidebar ----
        let mut pick = None;
        let mut new_pl = false;
        let mut open_pl_menu: Option<(egui::Pos2, usize)> = None;
        egui::Panel::left("playlists")
            .exact_size(230.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(12.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Image::new(egui::include_image!("../assets/temazoo-64.png")).fit_to_exact_size(egui::vec2(30.0, 30.0)));
                    ui.label(egui::RichText::new("Temazoo").color(ACCENT).strong().size(22.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if gear_button(ui, 20.0).on_hover_text("Configuración").clicked() {
                            self.show_settings = !self.show_settings;
                            self.cache_bytes = cache_size();
                        }
                    });
                });
                ui.add_space(12.0);
                for (label, fi) in [("Inicio", HOME), ("Explorar", EXPLORE)] {
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 34.0), egui::Sense::click());
                    let sel = self.page == Some(fi);
                    if sel || resp.hovered() {
                        let a = if sel { 45 } else { 18 };
                        ui.painter().rect_filled(rect, 8.0, Color32::from_rgba_unmultiplied(167, 139, 250, a));
                    }
                    ui.painter().text(
                        egui::pos2(rect.left() + 10.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        label,
                        egui::FontId::proportional(15.0),
                        if sel { ACCENT } else { Color32::from_gray(225) },
                    );
                    hand |= ui.rect_contains_pointer(rect);
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        nav_feed = Some(fi);
                    }
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("TUS PLAYLISTS").color(DIM).size(11.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if plus_button(ui).on_hover_text("Nueva playlist").clicked() {
                            new_pl = true;
                        }
                    });
                });
                ui.add_space(4.0);
                if self.needs_login {
                    ui.add_space(6.0);
                    let label = if self.logging_in { "Esperando inicio de sesión..." } else { "Iniciar sesión" };
                    if chip(ui, label, true).clicked() {
                        start_login = true;
                    }
                    ui.add_space(6.0);
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for (i, (_, title)) in self.playlists.iter().enumerate() {
                        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 34.0), egui::Sense::click());
                        let sel = self.sel_playlist == Some(i);
                        if sel || resp.hovered() {
                            let a = if sel { 45 } else { 18 };
                            ui.painter().rect_filled(rect, 8.0, Color32::from_rgba_unmultiplied(167, 139, 250, a));
                        }
                        let p = ui.painter().with_clip_rect(egui::Rect::from_min_max(rect.min, egui::pos2(rect.right() - 30.0, rect.bottom())));
                        p.text(
                            egui::pos2(rect.left() + 10.0, rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            title.replace('\u{a0}', " "),
                            egui::FontId::proportional(14.0),
                            if sel { ACCENT } else { Color32::from_gray(210) },
                        );
                        hand |= ui.rect_contains_pointer(rect);
                        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                        if let Some(pos) = row_menu_trigger(ui, rect, &resp) {
                            open_pl_menu = Some((pos, i));
                        } else if resp.clicked() {
                            pick = Some(i);
                        }
                    }
                });
            });
        if let Some(i) = pick {
            self.sel_playlist = Some(i);
            self.sel_id = Some(self.playlists[i].0.clone());
            self.page = None;
            self.is_search = false;
            self.view_title = None;
            self.cards.clear();
            self.history.clear();
            self.query.clear();
            self.status = "Cargando canciones...".into();
            let _ = self.cmd.send(Cmd::Tracks(self.playlists[i].0.clone()));
        }

        // ---- queue / lyrics ----
        // Lyrics are fetched only while their tab is open and the song changes (a few KB of text, nothing else).
        let want_lyrics = (self.show_queue && self.right_tab == RightTab::Lyrics) || (self.video_full && self.vfull_lyrics);
        if want_lyrics {
            if let Some(t) = &self.now {
                if self.lyrics.as_ref().is_none_or(|(id, _)| *id != t.id) {
                    self.lyrics = Some((t.id.clone(), Lyrics::Loading));
                    let _ = self.cmd.send(Cmd::Lyrics(t.id.clone(), t.title.clone(), t.artist.clone(), t.secs));
                }
            }
        }
        // Where the song is now, in ms (the audio position is a sample taken a moment ago).
        let now_ms = {
            let lag = if self.playing && !self.paused { pos_at.map_or(0, |t| t.elapsed().as_millis() as u64) } else { 0 };
            (pos.as_millis() as u64 + lag) as u32
        };
        let mut seek_to: Option<u32> = None;
        let mut new_line = self.lyr_line;
        if self.show_queue && !self.video_full {
            let mut tab = self.right_tab;
            egui::Panel::right("queue")
                .exact_size(330.0)
                .resizable(false)
                .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(12.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if chip(ui, "Cola", tab == RightTab::Queue).clicked() {
                            tab = RightTab::Queue;
                        }
                        if chip(ui, "Letra", tab == RightTab::Lyrics).clicked() {
                            tab = RightTab::Lyrics;
                        }
                        if tab == RightTab::Queue {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if chip(ui, "Limpiar", false).clicked() {
                                    acts.push(Act::QClear);
                                }
                            });
                        }
                    });
                    ui.add_space(6.0);
                    if tab == RightTab::Queue {
                        ui.horizontal(|ui| {
                            if switch(ui, self.autoplay).clicked() {
                                toggle_autoplay = true;
                            }
                            ui.label(egui::RichText::new("Sugerencias al terminar la cola").color(Color32::from_gray(200)).size(13.0));
                        });
                    }
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if switch(ui, self.show_video).clicked() {
                            toggle_video = true;
                        }
                        ui.label(egui::RichText::new("Mostrar video si existe").color(Color32::from_gray(200)).size(13.0));
                    });
                    if let Some(tex) = self.video_tex.as_ref().filter(|_| self.video.is_some()) {
                        ui.add_space(8.0);
                        let sz = tex.size_vec2();
                        let w = ui.available_width();
                        let img = ui.add(egui::Image::new(tex).fit_to_exact_size(egui::vec2(w, w * sz.y / sz.x)).corner_radius(8.0));
                        // Fullscreen button over the corner of the video.
                        let br = egui::Rect::from_center_size(img.rect.right_bottom() - egui::vec2(20.0, 20.0), egui::vec2(28.0, 28.0));
                        let r = ui.interact(br, ui.scope_id().with("vfull_btn"), egui::Sense::click());
                        ui.painter().rect_filled(br, 6.0, Color32::from_black_alpha(if r.hovered() { 200 } else { 130 }));
                        fs_glyph(ui.painter(), br.center(), Color32::WHITE, false);
                        if r.on_hover_text("Pantalla completa").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            enter_vfull = true;
                        }
                    } else if self.show_video && self.now.is_some() {
                        // No video for this song (or it has not arrived yet): show the cover in its place, so
                        // fullscreen is still available.
                        if let Some(n) = &self.now {
                            ui.add_space(8.0);
                            let w = ui.available_width();
                            let (rect, _) = ui.allocate_exact_size(egui::vec2(w, w * 9.0 / 16.0), egui::Sense::hover());
                            ui.painter().rect_filled(rect, 8.0, Color32::from_gray(18));
                            let side = rect.height();
                            cover(ui, &n.cover, egui::Rect::from_center_size(rect.center(), egui::vec2(side, side)), 8.0);
                            let br = egui::Rect::from_center_size(rect.right_bottom() - egui::vec2(20.0, 20.0), egui::vec2(28.0, 28.0));
                            let r = ui.interact(br, ui.scope_id().with("vfull_btn_cover"), egui::Sense::click());
                            ui.painter().rect_filled(br, 6.0, Color32::from_black_alpha(if r.hovered() { 200 } else { 130 }));
                            fs_glyph(ui.painter(), br.center(), Color32::WHITE, false);
                            if r.on_hover_text("Pantalla completa").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                enter_vfull = true;
                            }
                        }
                    }
                    ui.add_space(12.0);
                    if tab == RightTab::Lyrics {
                        self.lyrics_ui(ui, now_ms, false, &mut seek_to, &mut new_line);
                        return;
                    }
                    if let Some(n) = &self.now {
                        ui.label(egui::RichText::new("AHORA SUENA").color(DIM).size(11.0));
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 48.0), egui::Sense::hover());
                        cover(ui, &n.thumb, egui::Rect::from_min_size(rect.min + egui::vec2(6.0, 6.0), egui::vec2(36.0, 36.0)), 6.0);
                        let p = ui.painter().with_clip_rect(rect);
                        p.text(egui::pos2(rect.left() + 52.0, rect.center().y - 8.0), egui::Align2::LEFT_CENTER, &n.title, egui::FontId::proportional(14.0), ACCENT);
                        p.text(egui::pos2(rect.left() + 52.0, rect.center().y + 9.0), egui::Align2::LEFT_CENTER, &n.artist, egui::FontId::proportional(12.0), DIM);
                        ui.add_space(6.0);
                    }
                    let start = self.qpos.map_or(0, |p| p + 1);
                    let upcoming = self.queue.len().saturating_sub(start);
                    ui.label(egui::RichText::new("A CONTINUACIÓN").color(DIM).size(11.0));
                    if upcoming == 0 {
                        let hint = if self.autoplay { "Se agregarán sugerencias cuando termine la canción." } else { "No hay más canciones en la cola." };
                        ui.label(egui::RichText::new(hint).color(DIM).size(12.5));
                    }
                    let dragging = self.drag_from;
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, 48.0, upcoming, |ui, range| {
                        for k in range {
                            let qi = start + k;
                            let t = &self.queue[qi];
                            let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 48.0), egui::Sense::click());
                            let row_hover = ui.rect_contains_pointer(rect);
                            let being_dragged = dragging == Some(qi);
                            if being_dragged {
                                ui.painter().rect_filled(rect, 8.0, Color32::from_rgba_unmultiplied(167, 139, 250, 30));
                            } else if row_hover && dragging.is_none() {
                                ui.painter().rect_filled(rect, 8.0, Color32::from_rgba_unmultiplied(167, 139, 250, 14));
                            }
                            // Cover: click to play, like in YouTube Music.
                            let trect = egui::Rect::from_min_size(rect.min + egui::vec2(6.0, 6.0), egui::vec2(36.0, 36.0));
                            cover(ui, &t.thumb, trect, 6.0);
                            let timg = ui.interact(trect, resp.id.with("img"), egui::Sense::click());
                            if timg.hovered() && dragging.is_none() {
                                ui.painter().rect_filled(trect, 6.0, Color32::from_black_alpha(150));
                                let c = trect.center();
                                ui.painter().add(egui::Shape::convex_polygon(
                                    vec![egui::pos2(c.x - 5.0, c.y - 7.0), egui::pos2(c.x + 7.0, c.y), egui::pos2(c.x - 5.0, c.y + 7.0)],
                                    Color32::WHITE,
                                    egui::Stroke::NONE,
                                ));
                                hand = true;
                            }
                            let clip = egui::Rect::from_min_size(rect.min + egui::vec2(50.0, 0.0), egui::vec2((rect.width() - 112.0).max(40.0), 48.0));
                            let p = ui.painter().with_clip_rect(clip);
                            p.text(egui::pos2(clip.left(), rect.center().y - 8.0), egui::Align2::LEFT_CENTER, &t.title, egui::FontId::proportional(14.0), Color32::WHITE);
                            let sub = if t.sug { format!("{} · sugerida", t.artist) } else { t.artist.clone() };
                            p.text(egui::pos2(clip.left(), rect.center().y + 9.0), egui::Align2::LEFT_CENTER, sub, egui::FontId::proportional(12.0), DIM);
                            // Drag handle: appears on hover, drag it to reorder.
                            let hrect = egui::Rect::from_center_size(egui::pos2(rect.right() - 46.0, rect.center().y), egui::vec2(24.0, 24.0));
                            let h = ui.interact(hrect, resp.id.with("drag"), egui::Sense::drag());
                            if (row_hover && dragging.is_none()) || being_dragged {
                                move_icon(ui, hrect.center(), if h.hovered() || being_dragged { Color32::WHITE } else { DIM });
                            }
                            if h.hovered() && dragging.is_none() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                            }
                            if h.drag_started() {
                                drag_start = Some(qi);
                            }
                            if let Some(from) = dragging {
                                if qi != from && row_hover {
                                    drop_to = Some(qi);
                                    let y = if qi > from { rect.bottom() } else { rect.top() };
                                    ui.painter().line_segment(
                                        [egui::pos2(rect.left() + 6.0, y), egui::pos2(rect.right() - 6.0, y)],
                                        egui::Stroke::new(2.0, ACCENT),
                                    );
                                }
                            }
                            if dragging.is_some() {
                                continue;
                            }
                            if let Some(pos) = row_menu_trigger(ui, rect, &resp) {
                                open_menu = Some(Menu { pos, track: t.clone(), qidx: Some(qi), frame: frame_nr });
                            } else if timg.clicked() || resp.double_clicked() {
                                acts.push(Act::QJump(qi));
                            }
                        }
                    });
                });
            self.right_tab = tab;
            self.lyr_line = new_line;
            if let Some(ms) = seek_to {
                let _ = self.audio.send(Audio::Seek(Duration::from_millis(ms as u64)));
            }
        }

        // Queue drag & drop: start, preview under the pointer and drop.
        if let Some(i) = drag_start {
            self.drag_from = Some(i);
        }
        if let Some(from) = self.drag_from {
            if ctx.input(|i| i.pointer.any_released()) {
                if let Some(to) = drop_to {
                    acts.push(Act::QMove(from, to));
                }
                self.drag_from = None;
            } else {
                ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
                if let (Some(p), Some(t)) = (ctx.pointer_hover_pos(), self.queue.get(from)) {
                    let ghost = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, ui.scope_id().with("qghost")));
                    let r = egui::Rect::from_min_size(p + egui::vec2(14.0, -18.0), egui::vec2(250.0, 36.0));
                    ghost.rect_filled(r, 8.0, Color32::from_rgb(48, 42, 74));
                    ghost.with_clip_rect(r).text(
                        r.left_center() + egui::vec2(10.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        &t.title,
                        egui::FontId::proportional(13.0),
                        Color32::WHITE,
                    );
                }
            }
        }

        // ---- tracks ----
        let mut clicked = None;
        let mut do_search: Option<String> = None;
        let mut card_click = None;
        let mut go_back = false;
        let (mut do_update, mut dismiss_update) = (false, false);
        if let Some(rel) = &self.update {
            egui::Panel::top("update")
                .frame(egui::Frame::new().fill(ACCENT.gamma_multiply(0.25)).inner_margin(egui::Margin::symmetric(16, 8)))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!("Temazoo {} está disponible (tenés la {}).", rel.version, update::VERSION));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add_enabled(!self.updating, egui::Button::new("Más tarde")).clicked() {
                                dismiss_update = true;
                            }
                            let label = if self.updating { "Descargando..." } else { "Actualizar ahora" };
                            if ui.add_enabled(!self.updating, egui::Button::new(label)).clicked() {
                                do_update = true;
                            }
                        });
                    });
                });
        }
        if do_update {
            self.start_update(&ctx);
        }
        if dismiss_update {
            self.update = None;
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::symmetric(20, 16)))
            .show(ui, |ui| {
                let mut search_focus = false;
                let bar = egui::Frame::new()
                    .fill(Color32::from_gray(32))
                    .corner_radius(22.0)
                    .inner_margin(egui::Margin::symmetric(14, 8))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
                            magnifier(ui, r.center(), 9.0, DIM);
                            let w = (ui.available_width() - 26.0).max(60.0);
                            let resp = ui.add_sized(
                                [w, 22.0],
                                egui::TextEdit::singleline(&mut self.query)
                                    .hint_text("Buscar en la lista o en YouTube Music  (Ctrl+F)")
                                    .frame(egui::Frame::NONE),
                            );
                            if self.focus_search {
                                resp.request_focus();
                                self.focus_search = false;
                            }
                            search_focus = resp.has_focus();
                            if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                self.query.clear();
                            }
                            // Arrow keys move through the suggestions.
                            if resp.has_focus() && !self.suggest.is_empty() {
                                let n = self.suggest.len();
                                if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                                    self.suggest_sel = Some(self.suggest_sel.map_or(0, |s| (s + 1).min(n - 1)));
                                }
                                if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                                    self.suggest_sel = self.suggest_sel.and_then(|s| s.checked_sub(1));
                                }
                            }
                            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !self.query.trim().is_empty() {
                                if let Some(s) = self.suggest_sel.and_then(|i| self.suggest.get(i)).cloned() {
                                    self.query = s;
                                }
                                do_search = Some(self.query.trim().to_string());
                                resp.request_focus();
                            }
                            if !self.query.is_empty() && clear_button(ui).clicked() {
                                self.query.clear();
                            }
                        });
                    });
                // ---- autocomplete ----
                if self.query != self.last_query {
                    self.last_query = self.query.clone();
                    self.typed_at = std::time::Instant::now();
                    self.suggest_sel = None;
                }
                let typed = self.query.trim().to_string();
                if typed.is_empty() {
                    self.suggest.clear();
                    self.suggest_req.clear();
                } else if typed != self.suggest_req {
                    if self.typed_at.elapsed() >= Duration::from_millis(200) {
                        self.suggest_req = typed.clone();
                        let _ = self.cmd.send(Cmd::Suggest(typed.clone()));
                    } else {
                        ctx.request_repaint_after(Duration::from_millis(220));
                    }
                }
                let mut picked: Option<String> = None;
                if (search_focus || self.suggest_hover) && !self.suggest.is_empty() && !typed.is_empty() {
                    let r = bar.response.rect;
                    let area = egui::Area::new(ui.scope_id().with("suggest"))
                        .order(egui::Order::Foreground)
                        .fixed_pos(r.left_bottom() + egui::vec2(0.0, 4.0))
                        .show(&ctx, |ui| {
                            egui::Frame::popup(ui.style()).corner_radius(14.0).show(ui, |ui| {
                                ui.set_width(r.width() - 16.0);
                                for (i, s) in self.suggest.iter().enumerate() {
                                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), egui::Sense::click());
                                    if resp.hovered() || self.suggest_sel == Some(i) {
                                        ui.painter().rect_filled(rect, 8.0, Color32::from_rgba_unmultiplied(167, 139, 250, 30));
                                    }
                                    magnifier(ui, rect.left_center() + egui::vec2(14.0, 0.0), 7.0, DIM);
                                    ui.painter().with_clip_rect(rect).text(
                                        rect.left_center() + egui::vec2(32.0, 0.0),
                                        egui::Align2::LEFT_CENTER,
                                        s,
                                        egui::FontId::proportional(14.5),
                                        Color32::WHITE,
                                    );
                                    hand |= resp.hovered();
                                    if resp.clicked() {
                                        picked = Some(s.clone());
                                    }
                                }
                            });
                        });
                    self.suggest_hover = ctx.pointer_hover_pos().is_some_and(|p| area.response.rect.contains(p));
                } else {
                    self.suggest_hover = false;
                }
                if let Some(s) = picked {
                    self.query = s.clone();
                    self.last_query = s.clone();
                    self.suggest.clear();
                    self.suggest_req = s.clone();
                    do_search = Some(s);
                }
                // Search scope only applies while searching (typing, or showing results).
                if !self.query.trim().is_empty() || self.is_search {
                    ui.add_space(8.0);
                    let mut new_kind = None;
                    ui.horizontal(|ui| {
                        for (k, label) in [
                            (Kind::Songs, "Canciones"),
                            (Kind::Videos, "Videos"),
                            (Kind::Artists, "Artistas"),
                            (Kind::Albums, "Álbumes"),
                            (Kind::Playlists, "Listas"),
                        ] {
                            if chip(ui, label, self.kind == k).clicked() && self.kind != k {
                                new_kind = Some(k);
                            }
                        }
                    });
                    if let Some(k) = new_kind {
                        self.kind = k;
                        let q = if self.query.trim().is_empty() { self.search_q.clone() } else { self.query.trim().to_string() };
                        if !q.is_empty() {
                            do_search = Some(q);
                        }
                    }
                }
                ui.add_space(10.0);
                if !self.history.is_empty() && chip(ui, "< Volver", false).clicked() {
                    go_back = true;
                }
                // Home / Explore / category pages: shelves of cards and songs.
                if let (true, Some(fi)) = (self.query.trim().is_empty(), self.page) {
                    let feed = &self.feeds[fi];
                    ui.label(egui::RichText::new(&feed.title).strong().size(28.0));
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical().id_salt("feed").auto_shrink([false, false]).show(ui, |ui| {
                        for (si, shelf) in feed.shelves.iter().enumerate() {
                            ui.add_space(16.0);
                            if !shelf.title.is_empty() {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new(&shelf.title).strong().size(20.0));
                                    if shelf.kind != ShelfKind::Chips {
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            let x = self.shelf_x.entry((fi, si)).or_insert(0.0);
                                            if arrow_button(ui, true).clicked() {
                                                *x += 520.0;
                                            }
                                            if arrow_button(ui, false).clicked() {
                                                *x = (*x - 520.0).max(0.0);
                                            }
                                        });
                                    }
                                });
                                ui.add_space(8.0);
                            }
                            if shelf.kind == ShelfKind::Chips {
                                ui.horizontal_wrapped(|ui| {
                                    for (ii, item) in shelf.items.iter().enumerate() {
                                        if let FeedItem::Chip { title, .. } = item {
                                            if chip(ui, title, false).clicked() {
                                                feed_click = Some((fi, si, ii));
                                            }
                                        }
                                    }
                                });
                                continue;
                            }
                            let off = self.shelf_x.get(&(fi, si)).copied().unwrap_or(0.0);
                            let out = egui::ScrollArea::horizontal()
                                .id_salt(("shelf", fi, si))
                                .horizontal_scroll_offset(off)
                                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                                .show(ui, |ui| {
                                    ui.horizontal_top(|ui| {
                                        if shelf.kind == ShelfKind::Rows {
                                            let idx: Vec<usize> = (0..shelf.items.len()).collect();
                                            for col in idx.chunks(4) {
                                                ui.vertical(|ui| {
                                                    ui.set_width(360.0);
                                                    for &ii in col {
                                                        if let FeedItem::Song(t) = &shelf.items[ii] {
                                                            let is_cur = self.now.as_ref().is_some_and(|n| n.id == t.id);
                                                            let mut go_artist = false;
                                                            let resp = song_row(ui, t, 360.0, is_cur, &mut hand, &mut go_artist);
                                                            if go_artist {
                                                                acts.push(Act::GoArtist(t.artist_id.clone(), t.artist.clone()));
                                                            } else if resp.clicked() {
                                                                feed_click = Some((fi, si, ii));
                                                            }
                                                        }
                                                    }
                                                });
                                                ui.add_space(12.0);
                                            }
                                            return;
                                        }
                                        for (ii, item) in shelf.items.iter().enumerate() {
                                            let (title, sub, thumb, round) = match item {
                                                FeedItem::Card(c) => (&c.title, &c.subtitle, &c.thumb, c.kind == CardKind::Artist),
                                                FeedItem::Song(t) => (&t.title, &t.artist, &t.cover, false),
                                                FeedItem::Chip { .. } => continue,
                                            };
                                            let (rect, resp) = ui.allocate_exact_size(egui::vec2(160.0, 228.0), egui::Sense::click());
                                            let img = egui::Rect::from_min_size(rect.min, egui::vec2(160.0, 160.0));
                                            cover(ui, thumb, img, if round { 80.0 } else { 8.0 });
                                            let hovered = ui.rect_contains_pointer(rect);
                                            hand |= hovered;
                                            if hovered && !round {
                                                ui.painter().rect_filled(img, 8.0, Color32::from_black_alpha(100));
                                                ui.painter().circle_filled(img.center(), 22.0, Color32::WHITE);
                                                let c = img.center();
                                                ui.painter().add(egui::Shape::convex_polygon(
                                                    vec![egui::pos2(c.x - 6.0, c.y - 9.0), egui::pos2(c.x + 10.0, c.y), egui::pos2(c.x - 6.0, c.y + 9.0)],
                                                    Color32::BLACK,
                                                    egui::Stroke::NONE,
                                                ));
                                            }
                                            let clip = egui::Rect::from_min_size(rect.min + egui::vec2(0.0, 166.0), egui::vec2(160.0, 60.0));
                                            let p = ui.painter().with_clip_rect(clip);
                                            let x = if round { rect.center().x } else { rect.left() };
                                            let al = if round { egui::Align2::CENTER_TOP } else { egui::Align2::LEFT_TOP };
                                            p.text(egui::pos2(x, clip.top() + 2.0), al, title, egui::FontId::proportional(14.0), Color32::WHITE);
                                            p.text(egui::pos2(x, clip.top() + 22.0), al, sub, egui::FontId::proportional(12.0), DIM);
                                            if resp.clicked() {
                                                feed_click = Some((fi, si, ii));
                                            }
                                            ui.add_space(8.0);
                                        }
                                    });
                                });
                            self.shelf_x.insert((fi, si), out.state.clamped_offset().x);
                        }
                        // Sentinel: when it scrolls into view, fetch the next page of shelves.
                        if feed.next.is_some() || feed.loading || !feed.loaded {
                            ui.add_space(12.0);
                            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 40.0), egui::Sense::hover());
                            ui.put(rect, egui::Spinner::new().size(18.0).color(ACCENT));
                            if feed.next.is_some() && !feed.loading && ui.is_rect_visible(rect) {
                                more_feed = Some(fi);
                            }
                        }
                    });
                    return;
                }
                let title = if let Some(t) = &self.view_title {
                    t.clone()
                } else if self.is_search {
                    format!("Resultados para «{}»", self.search_q)
                } else {
                    self.sel_playlist
                        .and_then(|i| self.playlists.get(i))
                        .map(|p| p.1.clone())
                        .unwrap_or_else(|| "Elegí una playlist".into())
                };
                ui.label(egui::RichText::new(title).strong().size(28.0));
                let q = self.query.trim().to_lowercase();
                let idx: Vec<usize> = if q.is_empty() {
                    (0..self.tracks.len()).collect()
                } else {
                    (0..self.tracks.len())
                        .filter(|&i| {
                            let t = &self.tracks[i];
                            t.title.to_lowercase().contains(&q) || t.artist.to_lowercase().contains(&q)
                        })
                        .collect()
                };
                let pin = !q.is_empty();
                let sub = if pin { format!("{} coincidencias en la lista", idx.len()) } else { self.status.clone() };
                ui.label(egui::RichText::new(sub).color(DIM));
                ui.add_space(10.0);
                let mut crow: Vec<CRow> = Vec::new();
                let mut last_section = "";
                for (ci, c) in self.cards.iter().enumerate() {
                    if pin && !c.title.to_lowercase().contains(&q) && !c.subtitle.to_lowercase().contains(&q) {
                        continue;
                    }
                    if c.section != last_section && !c.section.is_empty() {
                        crow.push(CRow::Label(c.section.clone()));
                    }
                    last_section = &c.section;
                    crow.push(CRow::Card(ci));
                }
                // Artist page: a "Mostrar todo" row sits right under the top songs.
                let show_all = self.more_songs.is_some() && !pin;
                let rows = idx.len() + show_all as usize + crow.len() + pin as usize;
                egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, 52.0, rows, |ui, range| {
                    for r in range {
                        if pin && r == 0 {
                            let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 52.0), egui::Sense::click());
                            if resp.hovered() {
                                ui.painter().rect_filled(rect, 10.0, Color32::from_rgba_unmultiplied(167, 139, 250, 14));
                            }
                            let icon = egui::Rect::from_min_size(rect.min + egui::vec2(8.0, 6.0), egui::vec2(40.0, 40.0));
                            ui.painter().rect_filled(icon, 6.0, Color32::from_rgba_unmultiplied(167, 139, 250, 60));
                            magnifier(ui, icon.center(), 12.0, ACCENT);
                            ui.painter().with_clip_rect(rect).text(
                                egui::pos2(rect.left() + 60.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                format!("Buscar «{}» en YouTube Music", self.query.trim()),
                                egui::FontId::proportional(15.0),
                                ACCENT,
                            );
                            hand |= ui.rect_contains_pointer(rect);
                            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                do_search = Some(self.query.trim().to_string());
                            }
                            continue;
                        }
                        let k = r - pin as usize;
                        if show_all && k == idx.len() {
                            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 52.0), egui::Sense::hover());
                            let btn = egui::Rect::from_min_size(rect.left_center() + egui::vec2(8.0, -17.0), egui::vec2(128.0, 34.0));
                            let resp = ui.interact(btn, ui.scope_id().with("show_all"), egui::Sense::click());
                            let over = resp.hovered();
                            hand |= over;
                            if over {
                                ui.painter().rect_filled(btn, 17.0, Color32::from_white_alpha(14));
                            }
                            ui.painter().rect_stroke(btn, 17.0, egui::Stroke::new(1.0, Color32::from_gray(if over { 120 } else { 80 })), egui::StrokeKind::Inside);
                            ui.painter().text(btn.center(), egui::Align2::CENTER_CENTER, "Mostrar todo", egui::FontId::proportional(13.5), Color32::WHITE);
                            if resp.clicked() {
                                acts.push(Act::AllSongs);
                            }
                            continue;
                        }
                        let k = if show_all && k > idx.len() { k - 1 } else { k };
                        if k >= idx.len() {
                            match &crow[k - idx.len()] {
                                CRow::Label(s) => {
                                    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 52.0), egui::Sense::hover());
                                    ui.painter().text(
                                        egui::pos2(rect.left() + 8.0, rect.bottom() - 8.0),
                                        egui::Align2::LEFT_BOTTOM,
                                        s.to_uppercase(),
                                        egui::FontId::proportional(11.0),
                                        DIM,
                                    );
                                }
                                CRow::Card(ci) => {
                                    let c = &self.cards[*ci];
                                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 52.0), egui::Sense::click());
                                    if resp.hovered() {
                                        ui.painter().rect_filled(rect, 10.0, Color32::from_rgba_unmultiplied(167, 139, 250, 14));
                                    }
                                    let img = egui::Rect::from_min_size(rect.min + egui::vec2(8.0, 6.0), egui::vec2(40.0, 40.0));
                                    cover(ui, &c.thumb, img, if c.kind == CardKind::Artist { 20.0 } else { 6.0 });
                                    let clip = egui::Rect::from_min_size(rect.min + egui::vec2(60.0, 0.0), egui::vec2((rect.width() - 74.0).max(40.0), 52.0));
                                    let p = ui.painter().with_clip_rect(clip);
                                    p.text(egui::pos2(clip.left(), rect.center().y - 9.0), egui::Align2::LEFT_CENTER, &c.title, egui::FontId::proportional(15.0), Color32::WHITE);
                                    p.text(egui::pos2(clip.left(), rect.center().y + 10.0), egui::Align2::LEFT_CENTER, &c.subtitle, egui::FontId::proportional(12.5), DIM);
                                    hand |= ui.rect_contains_pointer(rect);
                                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                        card_click = Some(*ci);
                                    }
                                }
                            }
                            continue;
                        }
                        let i = idx[k];
                        let t = &self.tracks[i];
                        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 52.0), egui::Sense::click());
                        hand |= ui.rect_contains_pointer(rect);
                        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                        let is_cur = self.now.as_ref().is_some_and(|n| n.id == t.id);
                        if is_cur || resp.hovered() {
                            let a = if is_cur { 38 } else { 14 };
                            ui.painter().rect_filled(rect, 10.0, Color32::from_rgba_unmultiplied(167, 139, 250, a));
                        }
                        let img = egui::Rect::from_min_size(rect.min + egui::vec2(8.0, 6.0), egui::vec2(40.0, 40.0));
                        cover(ui, &t.thumb, img, 6.0);
                        let text_w = (rect.width() - 8.0 - 40.0 - 14.0 - 100.0).max(40.0);
                        let clip = egui::Rect::from_min_size(rect.min + egui::vec2(60.0, 0.0), egui::vec2(text_w, 52.0));
                        let p = ui.painter().with_clip_rect(clip);
                        p.text(
                            egui::pos2(clip.left(), rect.center().y - 9.0),
                            egui::Align2::LEFT_CENTER,
                            &t.title,
                            egui::FontId::proportional(15.0),
                            if is_cur { ACCENT } else { Color32::WHITE },
                        );
                        let go_artist = artist_line(ui, &resp, clip, rect.center().y + 10.0, t, 12.5);
                        if t.secs > 0 {
                            ui.painter().text(
                                egui::pos2(rect.right() - 40.0, rect.center().y),
                                egui::Align2::RIGHT_CENTER,
                                fmt(t.secs),
                                egui::FontId::proportional(13.0),
                                DIM,
                            );
                        }
                        if let Some(pos) = row_menu_trigger(ui, rect, &resp) {
                            open_menu = Some(Menu { pos, track: t.clone(), qidx: None, frame: frame_nr });
                        } else if go_artist {
                            acts.push(Act::GoArtist(t.artist_id.clone(), t.artist.clone()));
                        } else if resp.clicked() {
                            clicked = Some(i);
                        }
                    }
                });
            });
        if let Some(i) = clicked {
            self.play_index(i);
        }
        if let Some(q) = do_search {
            self.suggest.clear();
            self.suggest_req = q.clone();
            self.status = "Buscando...".into();
            self.pending = Some((q.clone(), self.kind));
            self.history.clear();
            let _ = self.cmd.send(Cmd::Search(q, self.kind));
        }
        if let Some(ci) = card_click {
            let c = self.cards[ci].clone();
            self.open_card(c);
        }
        if let Some((fi, si, ii)) = feed_click {
            match self.feeds[fi].shelves[si].items[ii].clone() {
                FeedItem::Card(c) => self.open_card(c),
                FeedItem::Chip { title, browse, params } => self.open_feed(title, browse, params),
                FeedItem::Song(t) => {
                    let shelf = &self.feeds[fi].shelves[si];
                    // "Quick picks"-style rows play as a list; a lone video card plays on its own.
                    let (queue, pos) = if shelf.kind == ShelfKind::Rows {
                        let songs: Vec<Track> = shelf.items.iter().filter_map(|i| if let FeedItem::Song(s) = i { Some(s.clone()) } else { None }).collect();
                        let pos = songs.iter().position(|s| s.id == t.id).unwrap_or(0);
                        (songs, pos)
                    } else {
                        (vec![t], 0)
                    };
                    self.start_queue(queue, pos);
                }
            }
        }
        if let Some(fi) = more_feed {
            self.load_feed(fi);
        }
        if let Some(fi) = nav_feed {
            self.page = Some(fi);
            self.sel_playlist = None;
            self.sel_id = None;
            self.is_search = false;
            self.view_title = None;
            self.history.clear();
            self.query.clear();
            if !self.feeds[fi].loaded {
                self.load_feed(fi);
            }
        }
        if go_back {
            if let Some(s) = self.history.pop() {
                self.page = s.page;
                self.status = format!("{} elementos", s.tracks.len() + s.cards.len());
                self.tracks = s.tracks;
                self.cards = s.cards;
                self.view_title = s.view_title;
                self.more_songs = s.more_songs;
                self.is_search = s.is_search;
                self.search_q = s.search_q;
                self.sel_playlist = s.sel_playlist;
                self.sel_id = s.sel_playlist.and_then(|i| self.playlists.get(i)).map(|p| p.0.clone());
                self.query.clear();
            }
        }

        // ---- track menu ----
        if open_menu.is_some() {
            self.menu = open_menu;
        }
        let mut close_menu = false;
        if let Some(m) = &self.menu {
            let sr = ctx.content_rect();
            let pos = egui::pos2(m.pos.x.min(sr.right() - 240.0).max(sr.left()), m.pos.y.min(sr.bottom() - 470.0).max(sr.top()));
            // Only offer "remove" while looking at one of our own playlists (not search results).
            let remove_target = self.sel_id.clone().filter(|id| !self.is_search && self.view_title.is_none() && self.owned.contains(id));
            let first = self.qpos.map_or(0, |p| p + 1);
            let area = egui::Area::new(ui.scope_id().with("track_menu"))
                .order(egui::Order::Foreground)
                .fixed_pos(pos)
                .show(&ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_width(220.0);
                        ui.add(egui::Label::new(egui::RichText::new(&m.track.title).strong().size(13.0)).truncate());
                        ui.add(egui::Label::new(egui::RichText::new(&m.track.artist).color(DIM).size(12.0)).truncate());
                        ui.separator();
                        match m.qidx {
                            None => {
                                if menu_item(ui, "Reproducir a continuación") {
                                    acts.push(Act::PlayNext(m.track.clone()));
                                    close_menu = true;
                                }
                                if menu_item(ui, "Agregar a la cola") {
                                    acts.push(Act::Enqueue(m.track.clone()));
                                    close_menu = true;
                                }
                            }
                            Some(qi) => {
                                if menu_item(ui, "Reproducir ahora") {
                                    acts.push(Act::QJump(qi));
                                    close_menu = true;
                                }
                                if qi > first && menu_item(ui, "Subir") {
                                    acts.push(Act::QUp(qi));
                                    close_menu = true;
                                }
                                if qi + 1 < self.queue.len() && menu_item(ui, "Bajar") {
                                    acts.push(Act::QDown(qi));
                                    close_menu = true;
                                }
                                if menu_item(ui, "Quitar de la cola") {
                                    acts.push(Act::QRemove(qi));
                                    close_menu = true;
                                }
                            }
                        }
                        if menu_item(ui, "Iniciar radio") {
                            acts.push(Act::StartRadio(m.track.clone()));
                            close_menu = true;
                        }
                        let liked = self.liked.contains(&m.track.id);
                        if menu_item(ui, if liked { "Quitar de Me gusta" } else { "Me gusta" }) {
                            acts.push(Act::Like(m.track.id.clone(), !liked));
                            close_menu = true;
                        }
                        if !m.track.artist_id.is_empty() && menu_item(ui, "Ir al artista") {
                            acts.push(Act::GoArtist(m.track.artist_id.clone(), m.track.artist.clone()));
                            close_menu = true;
                        }
                        if !m.track.album_id.is_empty() && menu_item(ui, "Ir al álbum") {
                            acts.push(Act::GoAlbum(m.track.album_id.clone(), m.track.album.clone()));
                            close_menu = true;
                        }
                        ui.separator();
                        ui.label(egui::RichText::new("AGREGAR A PLAYLIST").color(DIM).size(11.0));
                        egui::ScrollArea::vertical().max_height(170.0).show(ui, |ui| {
                            for (id, name) in self.playlists.iter().filter(|p| self.owned.contains(&p.0)) {
                                if menu_item(ui, &name.replace('\u{a0}', " ")) {
                                    acts.push(Act::AddTo(id.clone(), name.clone(), m.track.id.clone()));
                                    close_menu = true;
                                }
                            }
                        });
                        if menu_item(ui, "+ Nueva playlist con esta canción") {
                            acts.push(Act::NewWith(m.track.clone()));
                            close_menu = true;
                        }
                        if let (Some(pid), None, false) = (&remove_target, m.qidx, m.track.svid.is_empty()) {
                            ui.separator();
                            if menu_item(ui, "Quitar de esta playlist") {
                                acts.push(Act::RemoveFrom(pid.clone(), m.track.id.clone(), m.track.svid.clone()));
                                close_menu = true;
                            }
                        }
                    });
                });
            let outside = ctx.input(|i| i.pointer.any_pressed())
                && ctx.pointer_interact_pos().is_some_and(|p| !area.response.rect.contains(p))
                && frame_nr != m.frame;
            if outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                close_menu = true;
            }
        }
        if close_menu {
            self.menu = None;
        }
        for a in acts {
            self.act(a);
        }
        if start_login {
            self.start_login(&ctx);
        }
        if toggle_autoplay {
            self.autoplay = !self.autoplay;
            self.save_settings();
            self.ensure_suggestions();
        }
        if toggle_video {
            self.set_video(!self.show_video);
        }
        if toggle_shuffle {
            self.set_shuffle(!self.shuffle);
        }

        // ---- sidebar playlist menu ----
        if new_pl {
            self.dialog = Some(Dialog { kind: DlgKind::New { with: None }, name: String::new(), desc: String::new(), privacy: Priv::Private });
        }
        if let Some((pos, i)) = open_pl_menu {
            self.pl_menu = Some((pos, i, frame_nr));
        }
        let mut close_pl = false;
        if let Some((pos, i, opened)) = self.pl_menu {
            let (id, title) = self.playlists.get(i).cloned().unwrap_or_default();
            let owned = self.owned.contains(&id);
            let sr = ctx.content_rect();
            let pos = egui::pos2(pos.x.min(sr.right() - 240.0).max(sr.left()), pos.y.min(sr.bottom() - 140.0).max(sr.top()));
            let area = egui::Area::new(ui.scope_id().with("pl_menu"))
                .order(egui::Order::Foreground)
                .fixed_pos(pos)
                .show(&ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_width(220.0);
                        ui.add(egui::Label::new(egui::RichText::new(title.replace('\u{a0}', " ")).strong().size(13.0)).truncate());
                        ui.separator();
                        if owned {
                            if menu_item(ui, "Editar") {
                                let _ = self.cmd.send(Cmd::LoadEdit(id.clone()));
                                close_pl = true;
                            }
                            if menu_item(ui, "Eliminar") {
                                self.dialog = Some(Dialog { kind: DlgKind::Delete { id: id.clone() }, name: title.clone(), desc: String::new(), privacy: Priv::Private });
                                close_pl = true;
                            }
                        } else {
                            ui.label(egui::RichText::new("Solo lectura: no es una playlist tuya").color(DIM).size(12.5));
                        }
                    });
                });
            let outside = ctx.input(|i| i.pointer.any_pressed())
                && ctx.pointer_interact_pos().is_some_and(|p| !area.response.rect.contains(p))
                && frame_nr != opened;
            if outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                close_pl = true;
            }
        }
        if close_pl {
            self.pl_menu = None;
        }

        // ---- playlist dialogs ----
        if let Some(mut d) = self.dialog.take() {
            let (mut submit, mut cancel) = (false, ctx.input(|i| i.key_pressed(egui::Key::Escape)));
            let title = match d.kind {
                DlgKind::New { .. } => "Nueva playlist",
                DlgKind::Edit { .. } => "Editar playlist",
                DlgKind::Delete { .. } => "Eliminar playlist",
            };
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(&ctx, |ui| {
                    ui.set_width(340.0);
                    if matches!(d.kind, DlgKind::Delete { .. }) {
                        ui.label(format!("¿Eliminar «{}»?", d.name.replace('\u{a0}', " ")));
                        ui.label(egui::RichText::new("Esta acción no se puede deshacer.").color(DIM));
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            let del = egui::Button::new(egui::RichText::new("Eliminar").color(Color32::WHITE)).fill(Color32::from_rgb(190, 60, 60));
                            if ui.add(del).clicked() {
                                submit = true;
                            }
                            if ui.button("Cancelar").clicked() {
                                cancel = true;
                            }
                        });
                    } else {
                        ui.label("Nombre");
                        let r = ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(f32::INFINITY));
                        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            submit = true;
                        }
                        ui.add_space(6.0);
                        ui.label("Descripción");
                        ui.add(egui::TextEdit::multiline(&mut d.desc).desired_rows(3).desired_width(f32::INFINITY));
                        ui.add_space(6.0);
                        ui.label("Privacidad");
                        ui.horizontal(|ui| {
                            for (p, label) in [(Priv::Public, "Pública"), (Priv::Unlisted, "No listada"), (Priv::Private, "Privada")] {
                                if ui.radio(d.privacy == p, label).clicked() {
                                    d.privacy = p;
                                }
                            }
                        });
                        if let DlgKind::New { with: Some(t) } = &d.kind {
                            ui.label(egui::RichText::new(format!("Se agregará «{}».", t.title)).color(DIM).size(12.5));
                        }
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            let label = if matches!(d.kind, DlgKind::Edit { .. }) { "Guardar" } else { "Crear" };
                            if ui.add_enabled(!d.name.trim().is_empty(), egui::Button::new(label)).clicked() {
                                submit = true;
                            }
                            if ui.button("Cancelar").clicked() {
                                cancel = true;
                            }
                        });
                    }
                });
            let valid = matches!(d.kind, DlgKind::Delete { .. }) || !d.name.trim().is_empty();
            if submit && valid {
                let (name, desc) = (d.name.trim().to_string(), d.desc.trim().to_string());
                match &d.kind {
                    DlgKind::New { with } => {
                        let _ = self.cmd.send(Cmd::Create { title: name, desc, privacy: d.privacy, with: with.as_ref().map(|t| t.id.clone()) });
                    }
                    DlgKind::Edit { id } => {
                        let _ = self.cmd.send(Cmd::Edit { id: id.clone(), title: name, desc, privacy: d.privacy });
                    }
                    DlgKind::Delete { id } => {
                        if self.sel_id.as_ref() == Some(id) {
                            self.sel_id = None;
                            self.sel_playlist = None;
                            self.tracks.clear();
                            self.cards.clear();
                            self.view_title = None;
                        }
                        let _ = self.cmd.send(Cmd::DeletePl(id.clone()));
                    }
                }
                self.status = "Guardando...".into();
            } else if !cancel {
                self.dialog = Some(d);
            }
        }

        // ---- settings ----
        let mut check_now = false;
        let mut video_change: Option<bool> = None;
        let mut fsq_changed = false;
        let mut autoplay_changed = false;
        let mut open = self.show_settings;
        egui::Window::new("Configuración")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .fixed_size([680.0, 480.0])
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(&ctx, |ui| {
                let mut changed = false;
                ui.horizontal_top(|ui| {
                    // ---- navigation ----
                    ui.vertical(|ui| {
                        ui.set_width(150.0);
                        ui.set_min_height(430.0);
                        ui.add_space(4.0);
                        for (tab, label) in [
                            (SettingsTab::Playback, "Reproducción"),
                            (SettingsTab::Video, "Video"),
                            (SettingsTab::Lyrics, "Letra"),
                            (SettingsTab::Account, "Cuenta"),
                            (SettingsTab::Storage, "Almacenamiento"),
                            (SettingsTab::About, "Acerca de"),
                        ] {
                            if nav_item(ui, label, self.settings_tab == tab).clicked() {
                                self.settings_tab = tab;
                            }
                            ui.add_space(2.0);
                        }
                    });
                    ui.separator();
                    // ---- page ----
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width());
                        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                            ui.set_width(ui.available_width() - 8.0);
                            match self.settings_tab {
                                SettingsTab::Playback => {
                                    page_title(ui, "Reproducción");
                                    settings_card(ui, |ui| {
                                        setting_row(ui, "Formato de audio", "Opus tiene mejor calidad; AAC es más compatible. Ambos ~130 kbps. Se aplica a las próximas canciones.", |ui| {
                                            let cur = if self.quality == Quality::Opus { 0 } else { 1 };
                                            if let Some(i) = segmented(ui, &["Opus", "AAC"], cur) {
                                                self.quality = if i == 0 { Quality::Opus } else { Quality::Aac };
                                                changed = true;
                                            }
                                        });
                                        card_divider(ui);
                                        setting_row(ui, "Precargar la siguiente canción", "La baja de antemano para que no haya pausa entre canciones.", |ui| {
                                            if switch(ui, self.prefetch).clicked() {
                                                self.prefetch = !self.prefetch;
                                                changed = true;
                                            }
                                        });
                                        card_divider(ui);
                                        setting_row(ui, "Sugerencias al terminar la cola", "Cuando no queda nada, sigue con canciones parecidas.", |ui| {
                                            if switch(ui, self.autoplay).clicked() {
                                                self.autoplay = !self.autoplay;
                                                changed = true;
                                                autoplay_changed = true;
                                            }
                                        });
                                    });
                                }
                                SettingsTab::Video => {
                                    page_title(ui, "Video");
                                    settings_card(ui, |ui| {
                                        setting_row(ui, "Mostrar el video si existe", "Es el valor con el que arranca la app; también se cambia desde la Cola. Apagado no carga ni gasta nada.", |ui| {
                                            if switch(ui, self.video_default).clicked() {
                                                self.video_default = !self.video_default;
                                                changed = true;
                                                video_change = Some(self.video_default);
                                            }
                                        });
                                        card_divider(ui);
                                        setting_row(
                                            ui,
                                            "Calidad máxima en pantalla completa",
                                            "Más calidad usa más memoria mientras estás en pantalla completa: 480p ≈ +10 MB, 720p ≈ +20 MB, 1080p ≈ +40 MB. \"Pantalla\" usa la de tu pantalla, hasta 1080p. Solo si el video la ofrece.",
                                            |ui| {
                                                let opts = [(480u32, "480p"), (720, "720p"), (1080, "1080p"), (0, "Pantalla")];
                                                let cur = opts.iter().position(|(h, _)| *h == self.fs_quality).unwrap_or(1);
                                                let labels: Vec<&str> = opts.iter().map(|(_, l)| *l).collect();
                                                if let Some(i) = segmented(ui, &labels, cur) {
                                                    self.fs_quality = opts[i].0;
                                                    changed = true;
                                                    fsq_changed = true;
                                                }
                                            },
                                        );
                                    });
                                }
                                SettingsTab::Lyrics => {
                                    page_title(ui, "Letra en pantalla completa");
                                    settings_card(ui, |ui| {
                                        setting_row(ui, "Estilo", "\"Sobre el video\" muestra pocas líneas que se desvanecen (necesita letra sincronizada; si no la hay, usa el panel). También se cambia con clic derecho.", |ui| {
                                            let cur = if self.lyr_style == LyricsStyle::Overlay { 0 } else { 1 };
                                            if let Some(i) = segmented(ui, &["Sobre el video", "Panel lateral"], cur) {
                                                self.lyr_style = if i == 0 { LyricsStyle::Overlay } else { LyricsStyle::Panel };
                                                changed = true;
                                            }
                                        });
                                        card_divider(ui);
                                        ui.add_enabled_ui(self.lyr_style == LyricsStyle::Overlay, |ui| {
                                            setting_row(ui, "Líneas visibles", "Cuántas líneas se ven a la vez sobre el video.", |ui| {
                                                let opts = [1u8, 3, 5];
                                                let cur = opts.iter().position(|n| *n == self.lyr_lines).unwrap_or(1);
                                                if let Some(i) = segmented(ui, &["1", "3", "5"], cur) {
                                                    self.lyr_lines = opts[i];
                                                    changed = true;
                                                }
                                            });
                                        });
                                        card_divider(ui);
                                        setting_row(ui, "Mostrar al entrar en pantalla completa", "Si está apagado, se activa con el botón \"Letra\" o el clic derecho.", |ui| {
                                            if switch(ui, self.lyr_default_on).clicked() {
                                                self.lyr_default_on = !self.lyr_default_on;
                                                changed = true;
                                            }
                                        });
                                    });
                                    ui.label(egui::RichText::new("Las letras sincronizadas vienen de LRCLIB; si no hay, se usa el texto de YouTube Music.").color(DIM).size(12.0));
                                }
                                SettingsTab::Account => {
                                    page_title(ui, "Cuenta de YouTube Music");
                                    settings_card(ui, |ui| {
                                        if let Some(acc) = &self.account {
                                            ui.horizontal(|ui| {
                                                let (rect, _) = ui.allocate_exact_size(egui::vec2(48.0, 48.0), egui::Sense::hover());
                                                cover(ui, &acc.photo, rect, 24.0);
                                                ui.vertical(|ui| {
                                                    ui.label(egui::RichText::new(&acc.name).strong().size(16.0));
                                                    if !acc.handle.is_empty() {
                                                        ui.label(egui::RichText::new(&acc.handle).color(DIM).size(12.5));
                                                    }
                                                    ui.label(egui::RichText::new("● Sesión activa").color(Color32::from_rgb(110, 210, 140)).size(12.5));
                                                });
                                            });
                                        } else {
                                            ui.label(egui::RichText::new("No hay una sesión iniciada. Entrá con tu cuenta de Google para ver tu biblioteca, tus playlists y Me gusta.").color(DIM).size(13.0));
                                        }
                                        ui.add_space(10.0);
                                        let label = match (self.logging_in, self.account.is_some()) {
                                            (true, _) => "Esperando inicio de sesión...",
                                            (false, true) => "Cambiar de cuenta",
                                            (false, false) => "Iniciar sesión",
                                        };
                                        if ui.add_enabled(!self.logging_in, egui::Button::new(label)).clicked() {
                                            start_login = true;
                                        }
                                    });
                                    ui.label(egui::RichText::new("La sesión se guarda solo en esta computadora y se renueva sola.").color(DIM).size(12.0));
                                }
                                SettingsTab::Storage => {
                                    page_title(ui, "Almacenamiento");
                                    settings_card(ui, |ui| {
                                        setting_row(ui, "Caché de audio y video", &format!("{:.1} MB en la carpeta temporal. Se borra sola a los 7 días.", self.cache_bytes as f64 / 1_048_576.0), |ui| {
                                            if ui.button("Limpiar").clicked() {
                                                clear_cache();
                                                self.cache_bytes = cache_size();
                                            }
                                        });
                                        card_divider(ui);
                                        setting_row(ui, "Imágenes en memoria", "Carátulas ya decodificadas; se descartan las que no se ven.", |ui| {
                                            ui.label(egui::RichText::new(IMGS.lock().unwrap().order.len().to_string()).size(15.0));
                                        });
                                    });
                                }
                                SettingsTab::About => {
                                    page_title(ui, "Acerca de Temazoo");
                                    settings_card(ui, |ui| {
                                        setting_row(ui, &format!("Versión {}", update::VERSION), "Reproductor de YouTube Music liviano para Windows, hecho en Rust.", |ui| {
                                            let label = if self.checking_update { "Buscando..." } else { "Buscar actualizaciones" };
                                            if ui.add_enabled(!self.checking_update && !self.updating, egui::Button::new(label)).clicked() {
                                                check_now = true;
                                            }
                                        });
                                    });
                                    ui.label(egui::RichText::new("github.com/Fiambre/temazoo").color(DIM).size(12.5));
                                }
                            }
                        });
                    });
                });
                if changed {
                    self.save_settings();
                }
            });
        self.show_settings = open;
        if autoplay_changed {
            self.ensure_suggestions();
        }
        // A new maximum quality: forget the high-resolution file so the next request uses the new limit.
        if fsq_changed {
            self.video_hd = None;
            self.video_hd_asked = false;
            self.use_video_hd(&ctx, false);
        }
        if let Some(on) = video_change {
            self.set_video(on);
        }
        if check_now {
            self.status = "Buscando actualizaciones...".into();
            self.check_update(&ctx, true);
        }
        // ---- fullscreen video: covers the app above the player bar, which stays for the controls ----
        if self.video_full {
            let sr = ctx.content_rect();
            let rect = egui::Rect::from_min_max(sr.min, egui::pos2(sr.right(), sr.bottom() - 92.0));
            let info = if self.vfull_debug { self.debug_lines(rect.size() * ctx.pixels_per_point()) } else { Vec::new() };
            let mut toggle_debug = false;
            let mut toggle_lyrics = false;
            let (mut fs_seek, mut fs_line): (Option<u32>, usize) = (None, self.lyr_line);
            let lyrics_on = self.vfull_lyrics;
            // Overlay needs synced lyrics; plain ones (or the panel style) use the side panel. While they are
            // still loading in overlay style, nothing is drawn so the layout does not jump.
            let synced = matches!(self.lyrics, Some((_, Lyrics::Synced { .. })));
            let loading = !matches!(self.lyrics, Some((_, Lyrics::Plain { .. } | Lyrics::Synced { .. } | Lyrics::Missing)));
            let overlay = lyrics_on && self.lyr_style == LyricsStyle::Overlay && synced;
            let side_panel = lyrics_on && !overlay && !(self.lyr_style == LyricsStyle::Overlay && loading);
            let mut toggle_style = false;
            egui::Area::new(ui.scope_id().with("video_full")).order(egui::Order::Foreground).fixed_pos(rect.min).show(&ctx, |ui| {
                let (r, resp) = ui.allocate_exact_size(rect.size(), egui::Sense::click());
                ui.painter().rect_filled(r, 0.0, Color32::BLACK);
                // With the side panel, the picture takes the left part and the lyrics the right one.
                let split = r.left() + r.width() * 0.58;
                let media = if side_panel { egui::Rect::from_min_max(r.min, egui::pos2(split, r.bottom())) } else { r };
                let mut picture = media;
                if let Some(tex) = self.video_tex.as_ref().filter(|_| self.video.is_some()) {
                    let sz = tex.size_vec2();
                    let k = (media.width() / sz.x).min(media.height() / sz.y);
                    let img = egui::Rect::from_center_size(media.center(), sz * k);
                    picture = img;
                    ui.painter().image(tex.id(), img, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                } else if let Some(n) = &self.now {
                    // No video for this song: the cover, large.
                    let side = media.width().min(media.height()) * 0.8;
                    cover(ui, &resize(&n.cover, 720), egui::Rect::from_center_size(media.center(), egui::vec2(side, side)), 12.0);
                }
                if overlay {
                    // Over the picture itself (the video area; the whole screen when it is just the cover).
                    let over = if self.video.is_some() { picture } else { media };
                    self.lyrics_overlay(&ctx, ui.painter(), over, now_ms);
                }
                if side_panel {
                    let lp = egui::Rect::from_min_max(egui::pos2(split, r.top()), r.max);
                    ui.painter().rect_filled(lp, 0.0, Color32::from_black_alpha(150));
                    let inner = lp.shrink2(egui::vec2(28.0, 70.0));
                    ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
                        self.lyrics_ui(ui, now_ms, true, &mut fs_seek, &mut fs_line);
                    });
                }
                // Buttons, top right: debug info ("i") and exit.
                let br = egui::Rect::from_center_size(r.right_top() + egui::vec2(-30.0, 30.0), egui::vec2(32.0, 32.0));
                let b = ui.interact(br, ui.scope_id().with("vfull_exit"), egui::Sense::click());
                ui.painter().rect_filled(br, 6.0, Color32::from_black_alpha(if b.hovered() { 200 } else { 120 }));
                fs_glyph(ui.painter(), br.center(), Color32::WHITE, true);
                if b.on_hover_text("Salir (Esc)").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() || resp.double_clicked() {
                    exit_vfull = true;
                }
                let ib = br.translate(egui::vec2(-40.0, 0.0));
                let i = ui.interact(ib, ui.scope_id().with("vfull_info"), egui::Sense::click());
                let lit = self.vfull_debug;
                ui.painter().rect_filled(ib, 6.0, if lit { ACCENT } else { Color32::from_black_alpha(if i.hovered() { 200 } else { 120 }) });
                ui.painter().text(ib.center(), egui::Align2::CENTER_CENTER, "i", egui::FontId::proportional(18.0), Color32::WHITE);
                if i.on_hover_text("Información de depuración").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    toggle_debug = true;
                }
                let lb = egui::Rect::from_center_size(egui::pos2(ib.center().x - 56.0, ib.center().y), egui::vec2(64.0, 32.0));
                let l = ui.interact(lb, ui.scope_id().with("vfull_lyrics"), egui::Sense::click());
                ui.painter().rect_filled(lb, 6.0, if lyrics_on { ACCENT } else { Color32::from_black_alpha(if l.hovered() { 200 } else { 120 }) });
                ui.painter().text(lb.center(), egui::Align2::CENTER_CENTER, "Letra", egui::FontId::proportional(15.0), Color32::WHITE);
                if l.on_hover_text("Mostrar la letra").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    toggle_lyrics = true;
                }
                // Debug panel, top left.
                if !info.is_empty() {
                    let font = egui::FontId::monospace(13.0);
                    let galleys: Vec<_> = info.iter().map(|l| ui.painter().layout_no_wrap(l.clone(), font.clone(), Color32::from_gray(235))).collect();
                    let (w, h) = (galleys.iter().map(|g| g.size().x).fold(0.0, f32::max), galleys.iter().map(|g| g.size().y).sum::<f32>());
                    let bg = egui::Rect::from_min_size(r.left_top() + egui::vec2(16.0, 16.0), egui::vec2(w + 20.0, h + 20.0));
                    ui.painter().rect_filled(bg, 8.0, Color32::from_black_alpha(170));
                    let mut y = bg.top() + 10.0;
                    for g in galleys {
                        let hh = g.size().y;
                        ui.painter().galley(egui::pos2(bg.left() + 10.0, y), g, Color32::WHITE);
                        y += hh;
                    }
                }
                // Right click: context menu.
                resp.context_menu(|ui| {
                    if ui.button(if lyrics_on { "Ocultar la letra" } else { "Mostrar la letra" }).clicked() {
                        toggle_lyrics = true;
                        ui.close();
                    }
                    if ui.button(if self.lyr_style == LyricsStyle::Overlay { "Letra: pasar a panel lateral" } else { "Letra: pasar a sobre el video" }).clicked() {
                        toggle_style = true;
                        ui.close();
                    }
                    if ui.button(if self.vfull_debug { "Ocultar información de depuración" } else { "Información de depuración" }).clicked() {
                        toggle_debug = true;
                        ui.close();
                    }
                    if ui.button("Salir de pantalla completa").clicked() {
                        exit_vfull = true;
                        ui.close();
                    }
                });
            });
            if toggle_debug {
                self.vfull_debug = !self.vfull_debug;
            }
            if toggle_lyrics {
                self.vfull_lyrics = !self.vfull_lyrics;
            }
            if toggle_style {
                self.lyr_style = if self.lyr_style == LyricsStyle::Overlay { LyricsStyle::Panel } else { LyricsStyle::Overlay };
                self.vfull_lyrics = true;
                self.save_settings();
            }
            self.lyr_line = fs_line;
            if let Some(ms) = fs_seek {
                let _ = self.audio.send(Audio::Seek(Duration::from_millis(ms as u64)));
            }
        }
        if enter_vfull && !self.video_full {
            self.vfull_app_was_fs = app_fs;
            self.vfull_seen_fs = app_fs;
            self.video_full = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
            // (The higher resolution is requested by the per-frame check above, as soon as there is a video.)
        }
        if exit_vfull && self.video_full {
            self.video_full = false;
            // Back to the small player: the lighter file is enough there.
            self.use_video_hd(&ctx, false);
            if !self.vfull_app_was_fs {
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            }
        }
        if toggle_app_fs {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!app_fs));
        }
        evict_images(&ctx);
        if hand && self.dialog.is_none() && self.drag_from.is_none() {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }
}

fn main() -> eframe::Result<()> {
    enter_data_dir();
    // Child-process modes used to capture the Google session (see login.rs).
    match std::env::args().nth(1).as_deref() {
        Some("--login") => std::process::exit(login::run(true)),
        Some("--refresh") => std::process::exit(login::run(false)),
        _ => {}
    }
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Temazoo")
        .with_inner_size([1000.0, 640.0])
        .with_min_inner_size([760.0, 420.0]);
    // Window / taskbar icon.
    if let Ok(img) = image::load_from_memory(include_bytes!("../assets/temazoo-256.png")) {
        let img = img.to_rgba8();
        let (width, height) = img.dimensions();
        viewport = viewport.with_icon(egui::IconData { rgba: img.into_raw(), width, height });
    }
    let opts = eframe::NativeOptions { viewport, ..Default::default() };
    let res = eframe::run_native("Temazoo", opts, Box::new(|cc| Ok(Box::new(App::new(cc)))));
    kill_children();
    res
}



/// Working set of this process in MB (what the task manager shows).
fn process_mem_mb() -> Option<u64> {
    use windows::Win32::System::{
        ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::GetCurrentProcess,
    };
    unsafe {
        let mut c = PROCESS_MEMORY_COUNTERS { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
        K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb).as_bool().then(|| c.WorkingSetSize as u64 / 1_048_576)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lrc_lines_and_word_tags() {
        let l = parse_lrc("[ar:Someone]\n[00:18.89] <00:18.89>We're <00:19.25>no <00:19.53>strangers\n[01:02.5]Second\n[00:05.00][00:40.00] Chorus\n[00:50.00]");
        assert_eq!(l[0], (5000, "Chorus".to_string()));
        assert_eq!(l[1], (18890, "We're no strangers".to_string()));
        assert_eq!(l[2], (40000, "Chorus".to_string()));
        assert_eq!(l[3], (50000, String::new()));
        assert_eq!(l[4], (62500, "Second".to_string()));
        assert_eq!(l.len(), 5);
    }

    #[test]
    fn titles_lose_video_decoration() {
        assert_eq!(clean_title("Roots Bloody Roots (Official Video) [HD]"), "Roots Bloody Roots");
        assert_eq!(clean_title("Never Gonna Give You Up (Remastered 2022)"), "Never Gonna Give You Up");
        assert_eq!(clean_title("Song (feat. Someone)"), "Song (feat. Someone)");
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// Talks to lrclib.net: run with `cargo test --release live -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_lrclib() {
        let a = lrclib_synced("Never Gonna Give You Up (Official Video)", "Rick Astley", 213).await;
        println!("rick: {:?}", a.as_ref().map(|l| (l.len(), l[0].clone())));
        assert!(a.is_some_and(|l| l.len() > 20));
        let b = lrclib_synced("Roots Bloody Roots", "Sepultura Official", 212).await;
        println!("sepultura (channel as artist): {:?}", b.as_ref().map(|l| l.len()));
        assert!(b.is_some());
        let c = lrclib_synced("zzzz qqqq nonexistent song xyz", "nobody", 100).await;
        assert!(c.is_none());
    }
}
