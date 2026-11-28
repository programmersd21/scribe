//! the junk drawer: timestamps, widths, paths, and the osc 52 escape.
//!
//! deliberately boring. every function here takes a value and returns a value,
//! except clipboard encoding, which is just base64 with a fancy envelope.

use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthStr;

/// format centiseconds as `hh:mm:ss.mmm`. srt callers swap in a comma.
pub fn timestamp_cs(cs: i64) -> String {
    let cs = cs.max(0);
    let hours = cs / 360_000;
    let minutes = (cs / 6_000) % 60;
    let seconds = (cs / 100) % 60;
    let millis = (cs % 100) * 10;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

/// compact form for the transcript gutter: `mm:ss`.
pub fn timestamp_short(cs: i64) -> String {
    let cs = cs.max(0);
    let minutes = cs / 6_000;
    let seconds = (cs / 100) % 60;
    format!("{minutes:02}:{seconds:02}")
}

/// format a duration in seconds as `mm:ss` or `h:mm:ss`.
pub fn duration_short(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    let hours = s / 3600;
    let minutes = (s / 60) % 60;
    let secs = s % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}

/// bytes for humans. 1024-based, because nobody has ever been confused by
/// two different definitions of a megabyte. (everybody has.)
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["b", "kib", "mib", "gib", "tib"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} b")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// is this a url or a path? check the scheme, not the dots. a filename with a
/// dot in it is not a website, and a windows drive letter is definitely not a
/// protocol even if it looks like one.
///
pub fn is_url(s: &str) -> bool {
    parse_http_url(s).is_some()
}

/// just enough url parsing to reject junk without pulling in a url crate. we
/// care about scheme + host, not about the entire rfc. a filename with a colon
/// or a dot is still a filename.
fn valid_zone_id(zone: &str) -> bool {
    let bytes = zone.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            index += 1;
        } else if byte == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else {
            return false;
        }
    }
    !bytes.is_empty()
}

fn valid_ipvfuture(host: &str) -> bool {
    let versioned = host.strip_prefix('v').or_else(|| host.strip_prefix('V'));
    let Some(versioned) = versioned else {
        return false;
    };
    let Some((version, address)) = versioned.split_once('.') else {
        return false;
    };
    !version.is_empty()
        && version.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !address.is_empty()
        && address.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || b"-._~!$&'()*+,;=:".contains(&byte)
        })
}

fn valid_bracketed_host(host: &str) -> bool {
    if valid_ipvfuture(host) {
        return true;
    }
    if let Some((address, zone)) = host.split_once("%25") {
        address.parse::<std::net::Ipv6Addr>().is_ok() && valid_zone_id(zone)
    } else {
        host.parse::<std::net::Ipv6Addr>().is_ok()
    }
}

pub fn parse_http_url(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    let (scheme, rest) = s.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next()?;
    let host = if let Some(bracketed) = host_port.strip_prefix('[') {
        let (host, suffix) = bracketed.split_once(']')?;
        if !valid_bracketed_host(host) {
            return None;
        }
        if !suffix.is_empty() {
            let port = suffix.strip_prefix(':')?;
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            port.parse::<u16>().ok()?;
        }
        host
    } else {
        let (host, port) = match host_port.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        };
        if let Some(port) = port {
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            port.parse::<u16>().ok()?;
        }
        host
    };
    if host.is_empty() || host.chars().any(|c| c.is_whitespace()) {
        return None;
    }
    let normalized_host = if let Some((address, zone)) = host.split_once("%25") {
        format!("{}%25{zone}", address.to_ascii_lowercase())
    } else {
        host.to_ascii_lowercase()
    };
    Some((scheme.to_ascii_lowercase(), normalized_host))
}

fn host_matches(url: &str, hosts: &[&str]) -> bool {
    parse_http_url(url).is_some_and(|(_, host)| {
        hosts
            .iter()
            .any(|h| host == *h || host.ends_with(&format!(".{h}")))
    })
}

/// is this a youtube link? those get clickable timestamps in markdown export.
/// host matching, not substring matching: `notyoutube.com.evil.test` is not
/// youtube, however much it would like the markdown link to say otherwise.
pub fn is_youtube_url(s: &str) -> bool {
    host_matches(s, &["youtube.com", "youtu.be", "youtube-nocookie.com"])
}

/// dig the video id out of a youtube url. handles the three shapes people
/// actually paste: watch, youtu.be and embed/shorts.
pub fn youtube_id(s: &str) -> Option<String> {
    fn non_empty(value: &str) -> Option<String> {
        let value = value.split(['?', '#', '/']).next()?.trim();
        (!value.is_empty()).then(|| value.to_string())
    }
    let (_, host) = parse_http_url(s)?;
    let after_scheme = s.split_once("://")?.1;
    let after_host = after_scheme
        .find(['/', '?', '#'])
        .map(|i| &after_scheme[i..])
        .unwrap_or("");
    let (path_and_query, _) = after_host.split_once('#').unwrap_or((after_host, ""));
    let (path, query) = path_and_query
        .split_once('?')
        .unwrap_or((path_and_query, ""));
    if host == "youtu.be" || host.ends_with(".youtu.be") {
        return non_empty(path.trim_start_matches('/'));
    }
    if host == "youtube.com"
        || host.ends_with(".youtube.com")
        || host == "youtube-nocookie.com"
        || host.ends_with(".youtube-nocookie.com")
    {
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("v=") {
                return non_empty(v);
            }
        }
        for marker in ["embed", "shorts"] {
            if let Some(rest) = path
                .trim_start_matches('/')
                .strip_prefix(marker)
                .and_then(|p| p.strip_prefix('/'))
            {
                return non_empty(rest);
            }
        }
    }
    None
}

/// youtube timestamp link at a whole-second offset.
pub fn youtube_link(source: &str, cs: i64) -> Option<String> {
    let id = youtube_id(source)?;
    let secs = (cs.max(0) / 100) as u64;
    Some(format!("https://www.youtube.com/watch?v={id}&t={secs}s"))
}

/// `auto` plus the languages whisper actually knows. names are for humans;
/// `whisper_rs::get_lang_id` is the authority on whether a code is real.
pub const LANGUAGES: &[(&str, &str)] = &[
    ("auto", "detect automatically"),
    ("en", "english"),
    ("zh", "chinese"),
    ("de", "german"),
    ("es", "spanish"),
    ("ru", "russian"),
    ("ko", "korean"),
    ("fr", "french"),
    ("ja", "japanese"),
    ("pt", "portuguese"),
    ("tr", "turkish"),
    ("pl", "polish"),
    ("ca", "catalan"),
    ("nl", "dutch"),
    ("ar", "arabic"),
    ("sv", "swedish"),
    ("it", "italian"),
    ("id", "indonesian"),
    ("hi", "hindi"),
    ("fi", "finnish"),
    ("vi", "vietnamese"),
    ("he", "hebrew"),
    ("uk", "ukrainian"),
    ("el", "greek"),
    ("ms", "malay"),
    ("cs", "czech"),
    ("ro", "romanian"),
    ("da", "danish"),
    ("hu", "hungarian"),
    ("ta", "tamil"),
    ("no", "norwegian"),
    ("th", "thai"),
    ("ur", "urdu"),
    ("hr", "croatian"),
    ("bg", "bulgarian"),
    ("lt", "lithuanian"),
    ("la", "latin"),
    ("mi", "maori"),
    ("ml", "malayalam"),
    ("cy", "welsh"),
    ("sk", "slovak"),
    ("te", "telugu"),
    ("fa", "persian"),
    ("lv", "latvian"),
    ("bn", "bengali"),
    ("sr", "serbian"),
    ("az", "azerbaijani"),
    ("sl", "slovenian"),
    ("kn", "kannada"),
    ("et", "estonian"),
    ("mk", "macedonian"),
    ("br", "breton"),
    ("eu", "basque"),
    ("is", "icelandic"),
    ("hy", "armenian"),
    ("ne", "nepali"),
    ("mn", "mongolian"),
    ("bs", "bosnian"),
    ("kk", "kazakh"),
    ("sq", "albanian"),
    ("sw", "swahili"),
    ("gl", "galician"),
    ("mr", "marathi"),
    ("pa", "punjabi"),
    ("si", "sinhala"),
    ("km", "khmer"),
    ("my", "burmese"),
    ("am", "amharic"),
    ("yi", "yiddish"),
    ("lo", "lao"),
    ("dz", "dzongkha"),
    ("jv", "javanese"),
    ("su", "sundanese"),
];

pub fn validate_language(code: &str) -> Result<(), String> {
    if code == "auto" || LANGUAGES.iter().any(|(c, _)| *c == code) {
        return Ok(());
    }
    if whisper_rs::get_lang_id(code).is_some() {
        return Ok(());
    }
    Err(format!(
        "unknown language code '{code}'. try 'auto' or a two-letter code from whisper's language list"
    ))
}

pub fn language_name(code: &str) -> Option<&'static str> {
    LANGUAGES
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
}

/// where user data goes, without dragging in a directories crate. the last
/// resort is a temp dir, never the current working directory: dropping a
/// config in a random repo is worse than admitting we have no home directory.
pub fn cache_home() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg);
    }
    if cfg!(target_os = "macos")
        && let Some(home) = home_dir()
    {
        return home.join("Library").join("Caches");
    }
    if cfg!(windows)
        && let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty())
    {
        return PathBuf::from(local);
    }
    home_dir()
        .map(|h| h.join(".cache"))
        .unwrap_or_else(|| std::env::temp_dir().join("scribe-no-home"))
}

pub fn config_home() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg);
    }
    if cfg!(target_os = "macos")
        && let Some(home) = home_dir()
    {
        return home.join("Library").join("Application Support");
    }
    if cfg!(windows)
        && let Some(appdata) = std::env::var_os("APPDATA").filter(|v| !v.is_empty())
    {
        return PathBuf::from(appdata);
    }
    home_dir()
        .map(|h| h.join(".config"))
        .unwrap_or_else(|| std::env::temp_dir().join("scribe-no-home"))
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
}

/// expand `~` ourselves. the shell is not involved when a user pastes a path
/// into a text box, so somebody has to do it.
fn tilde_suffix(path: &str, windows: bool) -> Option<&str> {
    if windows {
        let rest = path.strip_prefix('~')?;
        if !rest.starts_with('/') && !rest.starts_with('\\') {
            return None;
        }
        Some(rest.trim_start_matches(['/', '\\']))
    } else {
        path.strip_prefix("~/")
            .map(|rest| rest.trim_start_matches('/'))
    }
}

fn with_trailing_separator(home: PathBuf, windows: bool) -> PathBuf {
    let mut value = home.into_os_string();
    let has_separator = {
        let text = value.to_string_lossy();
        text.ends_with('/') || text.ends_with('\\')
    };
    if !has_separator {
        value.push(if windows { "\\" } else { "/" });
    }
    PathBuf::from(value)
}

fn expand_tilde_from(path: &str, home: Option<PathBuf>, windows: bool) -> PathBuf {
    if path == "~" {
        return home
            .map(|home| with_trailing_separator(home, windows))
            .unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = tilde_suffix(path, windows)
        && let Some(home) = home
    {
        return if rest.is_empty() {
            with_trailing_separator(home, windows)
        } else {
            home.join(rest)
        };
    }
    PathBuf::from(path)
}

pub fn expand_tilde(path: &str) -> PathBuf {
    expand_tilde_from(path, home_dir(), cfg!(windows))
}

/// one path suggestion, with everything the UI needs to render and rank it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionCandidate {
    /// the text that will replace the input when accepted.
    pub value: String,
    /// just the file/directory name, for display in the dropdown.
    pub label: String,
    /// the directory portion, for rendering a dimmed path prefix.
    pub dir: String,
    pub is_dir: bool,
    /// lower is better; drives the ordering in the dropdown.
    pub score: i32,
}

/// match quality of `needle` against `haystack`, or None when it does not match.
/// lower scores are better. prefix beats substring beats fuzzy subsequence,
/// and a shorter haystack wins ties. this is the little bit of "intellisense
/// magic" that makes a partial name jump to the top of the list.
pub fn fuzzy_score(needle: &str, haystack: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let n = needle.to_ascii_lowercase();
    let h = haystack.to_ascii_lowercase();
    let hb = h.as_bytes();
    let nb = n.as_bytes();

    if h.starts_with(&n) {
        return Some(hb.len() as i32 - nb.len() as i32);
    }
    if let Some(pos) = h.find(&n) {
        return Some(100 + pos as i32 * 2 + hb.len() as i32 - nb.len() as i32);
    }

    // subsequence match: every char of needle appears in order.
    let mut ni = 0usize;
    let mut first = 0usize;
    let mut last = 0usize;
    let mut gaps = 0i32;
    for (i, &b) in hb.iter().enumerate() {
        if ni < nb.len() && b == nb[ni] {
            if ni == 0 {
                first = i;
            }
            if ni > 0 {
                gaps += (i - last - 1) as i32;
            }
            last = i;
            ni += 1;
        }
    }
    if ni == nb.len() {
        Some(300 + first as i32 * 2 + gaps + hb.len() as i32 - nb.len() as i32)
    } else {
        None
    }
}

/// media extensions we can actually feed to the decoder; give them a small
/// ranking nudge so `talk.wav` beats `talk.log` when both match.
fn extension_bonus(name: &str) -> i32 {
    let lower = name.to_ascii_lowercase();
    let media = [
        ".wav", ".mp3", ".m4a", ".flac", ".ogg", ".opus", ".webm", ".mp4", ".mkv", ".mov", ".aac",
        ".wma", ".aiff", ".aif", ".m4v", ".avi",
    ];
    if media.iter().any(|ext| lower.ends_with(ext)) {
        -20
    } else {
        0
    }
}

/// tab completion candidates, ranked. directories are kept even when the
/// prefix does not match exactly so you can keep drilling down. capped at 64
/// because nobody benefits from a list that fills the screen, and directory
/// reads are not free.
fn split_completion_path(input: &str, windows: bool) -> (&str, &str) {
    let separator = if windows {
        input.rfind(['/', '\\'])
    } else {
        input.rfind('/')
    };
    match separator {
        Some(i) => (&input[..=i], &input[i + 1..]),
        None if windows
            && input.as_bytes().get(1) == Some(&b':')
            && input.as_bytes()[0].is_ascii_alphabetic() =>
        {
            (&input[..2], &input[2..])
        }
        None => ("", input),
    }
}

fn is_windows_unc_path(input: &str, windows: bool) -> bool {
    windows
        && input.as_bytes().get(0).is_some_and(|byte| *byte == b'/' || *byte == b'\\')
        && input.as_bytes().get(1).is_some_and(|byte| *byte == b'/' || *byte == b'\\')
}

pub fn complete_path(input: &str) -> Vec<CompletionCandidate> {
    complete_path_with(input, cfg!(windows))
}

fn complete_path_with(input: &str, windows: bool) -> Vec<CompletionCandidate> {
    let raw = if input.starts_with('~') {
        expand_tilde_from(input, home_dir(), windows)
    } else {
        PathBuf::from(input)
    };
    let text = raw.to_string_lossy().to_string();
    if is_windows_unc_path(&text, windows) {
        return Vec::new();
    }
    let (dir, prefix) = split_completion_path(&text, windows);
    let scan = if dir.is_empty() { "." } else { dir };
    let scan = expand_tilde_from(scan, home_dir(), windows);
    let mut out: Vec<CompletionCandidate> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&scan) {
        let mut found: Vec<CompletionCandidate> = entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                let score = match fuzzy_score(prefix, &name) {
                    Some(s) => s,
                    None => return None,
                };
                let mut value = if dir.is_empty() {
                    name.clone()
                } else {
                    format!("{dir}{name}")
                };
                if is_dir && !value.ends_with('/') {
                    value.push('/');
                }
                let bonus = if is_dir { -10 } else { 0 } + extension_bonus(&name);
                Some(CompletionCandidate {
                    value,
                    label: name,
                    dir: dir.to_string(),
                    is_dir,
                    score: score + bonus,
                })
            })
            .collect();
        found.sort_by(|a, b| {
            a.score.cmp(&b.score).then_with(|| {
                a.label
                    .to_ascii_lowercase()
                    .cmp(&b.label.to_ascii_lowercase())
            })
        });
        out.extend(found.into_iter().take(64));
    }
    out
}

/// the longest string shared by every candidate value. used for incremental
/// completion: if all matches start with `foo`, typing `f` + tab fills in `foo`
/// instead of blindly committing the first suggestion.
pub fn common_prefix(candidates: &[CompletionCandidate]) -> String {
    let Some(first) = candidates.first() else {
        return String::new();
    };
    let mut prefix = first.value.clone();
    for c in candidates.iter().skip(1) {
        // bytes are fine for comparing, but cutting the prefix has to land on
        // a char boundary or rust will panic on perfectly valid unicode paths.
        let limit = prefix.len().min(c.value.len());
        let mut end = 0;
        for i in 0..limit {
            if prefix.as_bytes()[i] != c.value.as_bytes()[i] {
                break;
            }
            end = i + 1;
        }
        while !prefix.is_char_boundary(end) {
            end -= 1;
        }
        prefix.truncate(end);
    }
    prefix
}

/// strip terminal control sequences from untrusted text. whisper output,
/// remote titles and filenames all end up on screen; an escape sequence in any
/// of them can repaint the ui or, on a permissive terminal, do worse. newlines
/// and tabs are kept because transcripts legitimately contain them.
pub fn sanitize_terminal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\n' | '\t' => out.push(ch),
            '\u{1b}' => {
                // swallow escape parameters (0x20..0x3f) then the final byte.
                // '[31m' is three bytes; stopping at '[' would leave '31m'
                // painted into the transcript, which is not much better.
                match chars.next() {
                    Some(']') => {
                        for tail in chars.by_ref() {
                            if tail == '\u{7}' {
                                break;
                            }
                            if tail == '\u{1b}' {
                                let _ = chars.next();
                                break;
                            }
                        }
                    }
                    Some(_) => {
                        for next in chars.by_ref() {
                            if (0x20..=0x3f).contains(&(next as u32)) {
                                continue;
                            }
                            break;
                        }
                    }
                    None => {}
                }
            }
            c if (c as u32) < 0x20 || (0x7f..=0x9f).contains(&(c as u32)) => {}
            c => out.push(c),
        }
    }
    out
}

/// wrap to a cell width without splitting wide glyphs. a single word longer
/// than the line is split by character, because losing text is worse than an
/// ugly break.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut result = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut line_width = 0usize;
        for word in paragraph.split_whitespace() {
            let word_width = UnicodeWidthStr::width(word);
            if line_width > 0 && line_width + 1 + word_width <= width {
                line.push(' ');
                line.push_str(word);
                line_width += 1 + word_width;
                continue;
            }
            if line_width > 0 {
                result.push(std::mem::take(&mut line));
                line_width = 0;
            }
            if word_width <= width {
                line.push_str(word);
                line_width = word_width;
            } else {
                for ch in word.chars() {
                    let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    if line_width + cw > width && !line.is_empty() {
                        result.push(std::mem::take(&mut line));
                        line_width = 0;
                    }
                    line.push(ch);
                    line_width += cw;
                }
            }
        }
        result.push(line);
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

/// truncate or pad to an exact cell width. alignment is the entire point;
/// without this the columns jitter every time a word changes length.
pub fn pad_to(s: &str, width: usize) -> String {
    let w = UnicodeWidthStr::width(s);
    if w >= width {
        return truncate(s, width);
    }
    format!("{s}{}", " ".repeat(width - w))
}

/// truncate to at most `width` cells, adding a single ellipsis if needed.
pub fn truncate(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(s) <= width {
        return s.to_string();
    }
    let target = width.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > target {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

/// base64 for osc 52. works over ssh, unlike a clipboard crate that expects
/// a local x server. that is the whole reason this helper exists.
pub fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let bits = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((bits >> 18) & 63) as usize] as char);
        out.push(TABLE[((bits >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((bits >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(bits & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// the osc 52 escape itself. it goes to the terminal, not the os clipboard,
/// which is why it keeps working when you are ssh-ed into a box with no gui.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// where an export lands: beside the source, or in `--out` if you set it.
pub fn output_path(source: &str, out_dir: Option<&Path>, ext: &str) -> PathBuf {
    let stem = if is_url(source) {
        "transcript".to_string()
    } else {
        Path::new(source)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "transcript".to_string())
    };
    let file = format!("{stem}.{ext}");
    match out_dir {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(file),
        _ if is_url(source) => PathBuf::from(file),
        _ => Path::new(source).with_file_name(file),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(timestamp_cs(0), "00:00:00.000");
        assert_eq!(timestamp_cs(1234), "00:00:12.340");
        assert_eq!(timestamp_cs(3_661_234), "10:10:12.340");
        assert_eq!(timestamp_short(1234), "00:12");
        assert_eq!(timestamp_short(3_661_234), "610:12");
    }

    #[test]
    fn duration() {
        assert_eq!(duration_short(0.0), "00:00");
        assert_eq!(duration_short(65.0), "01:05");
        assert_eq!(duration_short(3661.0), "1:01:01");
    }

    #[test]
    fn byte_units() {
        assert_eq!(human_bytes(512), "512 b");
        assert_eq!(human_bytes(2048), "2.0 kib");
    }

    #[test]
    fn url_detection() {
        assert!(is_url("https://example.com/a"));
        assert!(is_url("HTTP://EXAMPLE.COM"));
        assert!(!is_url("/tmp/a.wav"));
        assert!(!is_url(r"c:\audio\file.wav"));
        assert!(!is_url("ftp://example.com/a"));
        assert!(!is_url("file:///tmp/a.wav"));
        assert!(!is_url("not a url"));
        assert!(!is_url("https:///no-host"));
        assert!(!is_url("http:// "));
        assert!(is_url("https://example.com/a?b=c#d"));
        assert!(is_url("http://user:pass@example.com:8080/a"));
        assert!(is_url("http://[::1]/audio.wav"));
        assert!(is_url("https://[2001:db8::1]:8443/audio.wav"));
        assert!(is_url("http://[fe80::1%25eth0]/audio.wav"));
        assert!(is_url("http://[fe80::1%25eth0]:8080/audio.wav"));
        assert_eq!(
            parse_http_url("http://[fe80::1%25Eth0]/audio.wav").unwrap().1,
            "fe80::1%25Eth0"
        );
        assert!(is_url("http://[v1.example]/audio.wav"));
        assert!(is_url("http://[Vf.a:b!$&'()*+,;=]/audio.wav"));
        assert!(!is_url("http://[not-ip]/audio.wav"));
        assert!(!is_url("http://[fe80::1%25]/audio.wav"));
        assert!(!is_url("http://[fe80::1%25bad%xx]/audio.wav"));
        assert!(!is_url("http://[fe80::1%25eth0%zz]/audio.wav"));
        assert!(!is_url("http://[fe80::1%25eth0]:99999/audio.wav"));
        assert!(!is_url("http://[v.example]/audio.wav"));
        assert!(!is_url("http://[v1.]/audio.wav"));
        assert!(!is_url("http://example.com:bad/audio.wav"));
        assert!(!is_url("http://example.com:/audio.wav"));
    }

    #[test]
    fn youtube_hosts_are_matched_as_hosts() {
        assert!(!is_youtube_url(
            "https://notyoutube.com.evil.test/watch?v=x"
        ));
        assert!(youtube_id("https://notyoutube.com.evil.test/watch?v=x").is_none());
        assert!(is_youtube_url("https://www.youtube.com/watch?v=abc"));
        assert_eq!(
            youtube_id("https://www.youtube.com/watch?v=abc"),
            Some("abc".into())
        );
        assert_eq!(
            youtube_id("https://youtube.com/embed/xyz?start=1"),
            Some("xyz".into())
        );
    }

    #[test]
    fn invalid_youtube_urls_do_not_produce_links() {
        assert!(youtube_id("https://youtu.be/").is_none());
        assert!(youtube_id("https://youtube.com/watch?x=1").is_none());
        assert!(youtube_link("https://example.com/a", 0).is_none());
    }

    #[test]
    fn fuzzy_ranking() {
        let exact = fuzzy_score("foo", "foobar.wav").unwrap();
        let sub = fuzzy_score("foo", "xfoobar.wav").unwrap();
        let fuzzy = fuzzy_score("foo", "f_a_o_o.wav").unwrap();
        assert!(exact < sub && sub < fuzzy);
        assert!(fuzzy_score("ab", "abc").unwrap() < fuzzy_score("ab", "abcdef").unwrap());
        assert_eq!(fuzzy_score("zzz", "foobar.wav"), None);
        assert_eq!(fuzzy_score("", "anything"), Some(0));
    }

    #[test]
    fn shared_prefix() {
        use super::CompletionCandidate as C;
        let list = vec![
            C {
                value: "foobar.wav".into(),
                label: "a".into(),
                dir: "".into(),
                is_dir: false,
                score: 0,
            },
            C {
                value: "foobaz.wav".into(),
                label: "b".into(),
                dir: "".into(),
                is_dir: false,
                score: 0,
            },
        ];
        assert_eq!(common_prefix(&list), "fooba");
        assert_eq!(common_prefix(&[]), "");
    }

    #[test]
    fn tilde_paths_accept_native_windows_separators() {
        assert_eq!(tilde_suffix("~/Documents", false), Some("Documents"));
        assert_eq!(tilde_suffix("~\\Documents", true), Some("Documents"));
        assert_eq!(tilde_suffix("~\\\\Documents", true), Some("Documents"));
        assert_eq!(tilde_suffix("~\\", true), Some(""));
        assert_eq!(tilde_suffix("~\\Documents", false), None);
    }

    #[test]
    fn tilde_root_keeps_a_trailing_separator_for_completion() {
        let home = PathBuf::from("C:\\Users\\person");
        let expanded = expand_tilde_from("~\\", Some(home), true);
        assert_eq!(expanded.to_string_lossy(), "C:\\Users\\person\\");
        let expanded_text = expanded.to_string_lossy();
        let (dir, prefix) = split_completion_path(&expanded_text, true);
        assert_eq!(dir, "C:\\Users\\person\\");
        assert_eq!(prefix, "");

        let expanded = expand_tilde_from("~", Some(PathBuf::from("C:\\Users\\person")), true);
        assert_eq!(expanded.to_string_lossy(), "C:\\Users\\person\\");
    }

    #[test]
    fn user_directories_are_platform_shaped() {
        let cache = cache_home();
        let config = config_home();
        assert!(!cache.as_os_str().is_empty());
        assert!(!config.as_os_str().is_empty());
        if cfg!(target_os = "macos") && home_dir().is_some() {
            assert!(cache.ends_with("Caches"));
            assert!(config.ends_with("Application Support"));
        }
        if cfg!(windows) && std::env::var_os("LOCALAPPDATA").is_none() && home_dir().is_some() {
            assert!(cache.ends_with(".cache"));
        }
    }

    #[test]
    fn terminal_text_is_sanitized() {
        let evil = "hello\x1b[31mred\x1b[0m\x07\x00world\nnext\tcol";
        let clean = sanitize_terminal(evil);
        assert!(!clean.contains('\u{1b}'));
        assert!(!clean.contains('\u{7}'));
        assert!(!clean.contains('\u{0}'));
        assert!(clean.contains("hellored"));
        assert!(clean.contains("world\nnext\tcol"));
        assert_eq!(sanitize_terminal("你好 世界"), "你好 世界");
    }

    #[test]
    fn shared_prefix_never_cuts_a_utf8_character() {
        use super::CompletionCandidate as C;
        let list = vec![
            C {
                value: "会议记录.wav".into(),
                label: "a".into(),
                dir: "".into(),
                is_dir: false,
                score: 0,
            },
            C {
                value: "会议纪要.wav".into(),
                label: "b".into(),
                dir: "".into(),
                is_dir: false,
                score: 0,
            },
        ];
        let shared = common_prefix(&list);
        assert!(std::str::from_utf8(shared.as_bytes()).is_ok());
        assert_eq!(shared, "会议");
        let mixed = vec![
            C {
                value: "éclair".into(),
                label: "a".into(),
                dir: "".into(),
                is_dir: false,
                score: 0,
            },
            C {
                value: "école".into(),
                label: "b".into(),
                dir: "".into(),
                is_dir: false,
                score: 0,
            },
        ];
        assert_eq!(common_prefix(&mixed), "éc");
    }

    #[test]
    fn completion_path_splits_native_and_windows_separators() {
        assert_eq!(
            split_completion_path("/home/user/rec", false),
            ("/home/user/", "rec")
        );
        assert_eq!(
            split_completion_path("C:\\Users\\user\\rec", true),
            ("C:\\Users\\user\\", "rec")
        );
        assert_eq!(
            split_completion_path("C:/Users/user/rec", true),
            ("C:/Users/user/", "rec")
        );
        assert_eq!(split_completion_path("C:rec", true), ("C:", "rec"));
        assert_eq!(split_completion_path("C:", true), ("C:", ""));
        assert!(is_windows_unc_path("\\\\server\\share", true));
        assert!(is_windows_unc_path("//server/share", true));
        assert!(is_windows_unc_path(r"/\\server/share", true));
        assert!(is_windows_unc_path(r"\//server/share", true));
        assert!(!is_windows_unc_path("C:\\Users\\person", true));
        assert!(!is_windows_unc_path("\\\\server\\share", false));
    }

    #[cfg(windows)]
    #[test]
    fn complete_path_uses_windows_separators_end_to_end() {
        let dir = std::env::temp_dir().join(format!("scribe-win-cmpl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("recording.wav"), b"x").unwrap();
        let input = format!("{}\\record", dir.display());
        let candidates = complete_path(&input);
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.value.ends_with("recording.wav"))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn complete_path_uses_windows_separator_mode_end_to_end() {
        let dir = std::env::temp_dir().join(format!("scribe-cmpl-win-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("recording.wav"), b"x").unwrap();

        let input = format!("{}/record", dir.display());
        let candidates = complete_path_with(&input, true);

        assert!(candidates.iter().any(|candidate| candidate.value.ends_with("recording.wav")));
        assert!(complete_path_with(r"\\\\server\\share", true).is_empty());
        assert!(complete_path_with(r"/\\server/share", true).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn complete_ranks_and_marks_dirs() {
        let dir = std::env::temp_dir().join(format!("scribe_cmpl_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("subdir"));
        let _ = std::fs::write(dir.join("talk.wav"), b"x");
        let _ = std::fs::write(dir.join("talk.log"), b"x");

        let input = format!("{}/tal", dir.display());
        let list = complete_path(&input);
        assert!(!list.is_empty());
        let wav = list
            .iter()
            .position(|c| c.value.ends_with("talk.wav"))
            .unwrap();
        let log = list
            .iter()
            .position(|c| c.value.ends_with("talk.log"))
            .unwrap();
        assert!(wav < log);

        let dirs = complete_path(&format!("{}/sub", dir.display()));
        assert!(dirs[0].is_dir);
        assert!(dirs[0].value.ends_with('/'));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn youtube_links() {
        assert_eq!(youtube_id("https://youtu.be/abc123"), Some("abc123".into()));
        assert_eq!(
            youtube_id("https://youtube.com/watch?v=xyz&t=4"),
            Some("xyz".into())
        );
        assert!(is_youtube_url("https://youtu.be/abc123"));
        assert_eq!(
            youtube_link("https://youtu.be/abc123", 6500),
            Some("https://www.youtube.com/watch?v=abc123&t=65s".into())
        );
        assert_eq!(youtube_link("https://example.com/a", 0), None);
    }

    #[test]
    fn wrapping_unicode() {
        let lines = wrap("你好 世界 test", 8);
        assert!(
            lines
                .iter()
                .all(|l| UnicodeWidthStr::width(l.as_str()) <= 8)
        );
        assert!(wrap("", 5) == vec![""]);
    }

    #[test]
    fn truncate_and_pad() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(pad_to("ab", 5), "ab   ");
        assert_eq!(truncate("ab", 5), "ab");
    }

    #[test]
    fn base64_osc52() {
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(base64(b""), "");
        assert!(osc52("hi").starts_with("\x1b]52;c;aGk=\x07"));
    }

    #[test]
    fn output_naming() {
        let out = output_path("/tmp/meeting.wav", None, "srt");
        assert_eq!(out, PathBuf::from("/tmp/meeting.srt"));
        let out = output_path("https://x/y", None, "txt");
        assert_eq!(out, PathBuf::from("transcript.txt"));
    }
}
