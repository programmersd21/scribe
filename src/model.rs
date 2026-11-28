//! finding models, downloading models, and swearing at interrupted downloads.
//!
//! the catalog mirrors every pre-converted ggml model published in the
//! ggerganov/whisper.cpp hugging face repo, including the quantized q5/q8
//! builds and the english-only `.en` variants. sizes and shas come from the
//! hugging face file metadata, not the stale table in the repo readme.

use crate::util::{expand_tilde, human_bytes};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime};

pub const BASE_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    pub key: &'static str,
    pub file: &'static str,
    pub label: &'static str,
    pub size_mib: u64,
    pub ram_mib: u64,
    pub speed: &'static str,
    /// the tag shown in the picker. "default" and "quality" are the ones
    /// i actually recommend; the rest are there for people with opinions.
    pub note: &'static str,
    /// the sha256 from the upstream model table, shown in the picker and used
    /// to verify a fully downloaded file.
    pub sha: &'static str,
    /// true unless the name carries `.en` (english-only builds).
    pub multilingual: bool,
}

pub const CATALOG: &[ModelInfo] = &[
    ModelInfo {
        key: "tiny",
        file: "ggml-tiny.bin",
        label: "tiny",
        size_mib: 75,
        ram_mib: 200,
        speed: "fastest",
        note: "classic",
        sha: "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21",
        multilingual: true,
    },
    ModelInfo {
        key: "tiny-q5_1",
        file: "ggml-tiny-q5_1.bin",
        label: "tiny q5_1",
        size_mib: 31,
        ram_mib: 120,
        speed: "fastest",
        note: "lite",
        sha: "818710568da3ca15689e31a743197b520007872ff9576237bda97bd1b469c3d7",
        multilingual: true,
    },
    ModelInfo {
        key: "tiny-q8_0",
        file: "ggml-tiny-q8_0.bin",
        label: "tiny q8_0",
        size_mib: 42,
        ram_mib: 120,
        speed: "fastest",
        note: "quality",
        sha: "c2085835d3f50733e2ff6e4b41ae8a2b8d8110461e18821b09a15c40c42d1cca",
        multilingual: true,
    },
    ModelInfo {
        key: "tiny.en",
        file: "ggml-tiny.en.bin",
        label: "tiny.en",
        size_mib: 75,
        ram_mib: 200,
        speed: "fastest",
        note: "english",
        sha: "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f",
        multilingual: false,
    },
    ModelInfo {
        key: "tiny.en-q5_1",
        file: "ggml-tiny.en-q5_1.bin",
        label: "tiny.en q5_1",
        size_mib: 31,
        ram_mib: 120,
        speed: "fastest",
        note: "english lite",
        sha: "c77c5766f1cef09b6b7d47f21b546cbddd4157886b3b5d6d4f709e91e66c7c2b",
        multilingual: false,
    },
    ModelInfo {
        key: "tiny.en-q8_0",
        file: "ggml-tiny.en-q8_0.bin",
        label: "tiny.en q8_0",
        size_mib: 42,
        ram_mib: 120,
        speed: "fastest",
        note: "english",
        sha: "5bc2b3860aa151a4c6e7bb095e1fcce7cf12c7b020ca08dcec0c6d018bb7dd94",
        multilingual: false,
    },
    ModelInfo {
        key: "base",
        file: "ggml-base.bin",
        label: "base",
        size_mib: 142,
        ram_mib: 500,
        speed: "fast",
        note: "classic",
        sha: "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe",
        multilingual: true,
    },
    ModelInfo {
        key: "base-q5_1",
        file: "ggml-base-q5_1.bin",
        label: "base q5_1",
        size_mib: 57,
        ram_mib: 200,
        speed: "fast",
        note: "lite",
        sha: "422f1ae452ade6f30a004d7e5c6a43195e4433bc370bf23fac9cc591f01a8898",
        multilingual: true,
    },
    ModelInfo {
        key: "base-q8_0",
        file: "ggml-base-q8_0.bin",
        label: "base q8_0",
        size_mib: 78,
        ram_mib: 200,
        speed: "fast",
        note: "quality",
        sha: "c577b9a86e7e048a0b7eada054f4dd79a56bbfa911fbdacf900ac5b567cbb7d9",
        multilingual: true,
    },
    ModelInfo {
        key: "base.en",
        file: "ggml-base.en.bin",
        label: "base.en",
        size_mib: 142,
        ram_mib: 500,
        speed: "fast",
        note: "english",
        sha: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
        multilingual: false,
    },
    ModelInfo {
        key: "base.en-q5_1",
        file: "ggml-base.en-q5_1.bin",
        label: "base.en q5_1",
        size_mib: 57,
        ram_mib: 200,
        speed: "fast",
        note: "english lite",
        sha: "4baf70dd0d7c4247ba2b81fafd9c01005ac77c2f9ef064e00dcf195d0e2fdd2f",
        multilingual: false,
    },
    ModelInfo {
        key: "base.en-q8_0",
        file: "ggml-base.en-q8_0.bin",
        label: "base.en q8_0",
        size_mib: 78,
        ram_mib: 200,
        speed: "fast",
        note: "english",
        sha: "a4d4a0768075e13cfd7e19df3ae2dbc4a68d37d36a7dad45e8410c9a34f8c87e",
        multilingual: false,
    },
    ModelInfo {
        key: "small",
        file: "ggml-small.bin",
        label: "small",
        size_mib: 466,
        ram_mib: 1200,
        speed: "balanced",
        note: "classic",
        sha: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
        multilingual: true,
    },
    ModelInfo {
        key: "small-q5_1",
        file: "ggml-small-q5_1.bin",
        label: "small q5_1",
        size_mib: 181,
        ram_mib: 500,
        speed: "balanced",
        note: "default",
        sha: "ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb",
        multilingual: true,
    },
    ModelInfo {
        key: "small-q8_0",
        file: "ggml-small-q8_0.bin",
        label: "small q8_0",
        size_mib: 252,
        ram_mib: 650,
        speed: "balanced",
        note: "quality",
        sha: "49c8fb02b65e6049d5fa6c04f81f53b867b5ec9540406812c643f177317f779f",
        multilingual: true,
    },
    ModelInfo {
        key: "small.en",
        file: "ggml-small.en.bin",
        label: "small.en",
        size_mib: 466,
        ram_mib: 1200,
        speed: "balanced",
        note: "english",
        sha: "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d",
        multilingual: false,
    },
    ModelInfo {
        key: "small.en-q5_1",
        file: "ggml-small.en-q5_1.bin",
        label: "small.en q5_1",
        size_mib: 181,
        ram_mib: 500,
        speed: "balanced",
        note: "english lite",
        sha: "bfdff4894dcb76bbf647d56263ea2a96645423f1669176f4844a1bf8e478ad30",
        multilingual: false,
    },
    ModelInfo {
        key: "small.en-q8_0",
        file: "ggml-small.en-q8_0.bin",
        label: "small.en q8_0",
        size_mib: 252,
        ram_mib: 650,
        speed: "balanced",
        note: "english",
        sha: "67a179f608ea6114bd3fdb9060e762b588a3fb3bd00c4387971be4d177958067",
        multilingual: false,
    },
    ModelInfo {
        key: "medium",
        file: "ggml-medium.bin",
        label: "medium",
        size_mib: 1536,
        ram_mib: 3200,
        speed: "accurate",
        note: "classic",
        sha: "6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208",
        multilingual: true,
    },
    ModelInfo {
        key: "medium-q5_0",
        file: "ggml-medium-q5_0.bin",
        label: "medium q5_0",
        size_mib: 514,
        ram_mib: 1200,
        speed: "accurate",
        note: "extra",
        sha: "19fea4b380c3a618ec4723c3eef2eb785ffba0d0538cf43f8f235e7b3b34220f",
        multilingual: true,
    },
    ModelInfo {
        key: "medium-q8_0",
        file: "ggml-medium-q8_0.bin",
        label: "medium q8_0",
        size_mib: 785,
        ram_mib: 1600,
        speed: "accurate",
        note: "quality",
        sha: "42a1ffcbe4167d224232443396968db4d02d4e8e87e213d3ee2e03095dea6502",
        multilingual: true,
    },
    ModelInfo {
        key: "medium.en",
        file: "ggml-medium.en.bin",
        label: "medium.en",
        size_mib: 1536,
        ram_mib: 3200,
        speed: "accurate",
        note: "english",
        sha: "cc37e93478338ec7700281a7ac30a10128929eb8f427dda2e865faa8f6da4356",
        multilingual: false,
    },
    ModelInfo {
        key: "medium.en-q5_0",
        file: "ggml-medium.en-q5_0.bin",
        label: "medium.en q5_0",
        size_mib: 514,
        ram_mib: 1200,
        speed: "accurate",
        note: "english lite",
        sha: "76733e26ad8fe1c7a5bf7531a9d41917b2adc0f20f2e4f5531688a8c6cd88eb0",
        multilingual: false,
    },
    ModelInfo {
        key: "medium.en-q8_0",
        file: "ggml-medium.en-q8_0.bin",
        label: "medium.en q8_0",
        size_mib: 785,
        ram_mib: 1600,
        speed: "accurate",
        note: "english",
        sha: "43fa2cd084de5a04399a896a9a7a786064e221365c01700cea4666005218f11c",
        multilingual: false,
    },
    ModelInfo {
        key: "large-v1",
        file: "ggml-large-v1.bin",
        label: "large-v1",
        size_mib: 2969,
        ram_mib: 3200,
        speed: "slow",
        note: "legacy",
        sha: "7d99f41a10525d0206bddadd86760181fa920438b6b33237e3118ff6c83bb53d",
        multilingual: true,
    },
    ModelInfo {
        key: "large-v2",
        file: "ggml-large-v2.bin",
        label: "large-v2",
        size_mib: 2969,
        ram_mib: 3200,
        speed: "slow",
        note: "classic",
        sha: "9a423fe4d40c82774b6af34115b8b935f34152246eb19e80e376071d3f999487",
        multilingual: true,
    },
    ModelInfo {
        key: "large-v2-q5_0",
        file: "ggml-large-v2-q5_0.bin",
        label: "large-v2 q5_0",
        size_mib: 1126,
        ram_mib: 2100,
        speed: "slow",
        note: "extra",
        sha: "3a214837221e4530dbc1fe8d734f302af393eb30bd0ed046042ebf4baf70f6f2",
        multilingual: true,
    },
    ModelInfo {
        key: "large-v2-q8_0",
        file: "ggml-large-v2-q8_0.bin",
        label: "large-v2 q8_0",
        size_mib: 1536,
        ram_mib: 3200,
        speed: "slow",
        note: "quality",
        sha: "fef54e6d898246a65c8285bfa83bd1807e27fadf54d5d4e81754c47634737e8c",
        multilingual: true,
    },
    ModelInfo {
        key: "large-v3",
        file: "ggml-large-v3.bin",
        label: "large-v3",
        size_mib: 2969,
        ram_mib: 3200,
        speed: "slow",
        note: "best",
        sha: "64d182b440b98d5203c4f9bd541544d84c605196c4f7b845dfa11fb23594d1e2",
        multilingual: true,
    },
    ModelInfo {
        key: "large-v3-q5_0",
        file: "ggml-large-v3-q5_0.bin",
        label: "large-v3 q5_0",
        size_mib: 1126,
        ram_mib: 2100,
        speed: "slow",
        note: "extra",
        sha: "d75795ecff3f83b5faa89d1900604ad8c780abd5739fae406de19f23ecd98ad1",
        multilingual: true,
    },
    ModelInfo {
        key: "large-v3-turbo",
        file: "ggml-large-v3-turbo.bin",
        label: "large-v3-turbo",
        size_mib: 1536,
        ram_mib: 3200,
        speed: "accurate",
        note: "quality",
        sha: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
        multilingual: true,
    },
    ModelInfo {
        key: "large-v3-turbo-q5_0",
        file: "ggml-large-v3-turbo-q5_0.bin",
        label: "large-v3-turbo q5_0",
        size_mib: 547,
        ram_mib: 1200,
        speed: "accurate",
        note: "quality",
        sha: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
        multilingual: true,
    },
    ModelInfo {
        key: "large-v3-turbo-q8_0",
        file: "ggml-large-v3-turbo-q8_0.bin",
        label: "large-v3-turbo q8_0",
        size_mib: 834,
        ram_mib: 1600,
        speed: "accurate",
        note: "quality",
        sha: "317eb69c11673c9de1e1f0d459b253999804ec71ac4c23c17ecf5fbe24e259a1",
        multilingual: true,
    },
];

/// the model we reach for when nobody told us what to use. pinned by key so
/// the catalog can stay in alphabetical order for easy scanning.
pub fn default_model() -> &'static ModelInfo {
    find("small-q5_1").unwrap_or(&CATALOG[0])
}

pub fn find(key: &str) -> Option<&'static ModelInfo> {
    CATALOG.iter().find(|m| m.key == key)
}

/// where models live. xdg if you have it, ~/.cache if you do not. i am not
/// going to guess three more conventions and make a mess of your home dir.
pub fn cache_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(dir) = TEST_CACHE_DIR.with(|dir| dir.borrow().clone()) {
        return dir;
    }
    crate::util::cache_home().join("scribe").join("models")
}

#[cfg(test)]
thread_local! {
    static TEST_CACHE_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_test_cache_dir(dir: Option<PathBuf>) {
    TEST_CACHE_DIR.with(|cache| *cache.borrow_mut() = dir);
}

/// What a `--model`/saved value can refer to. Keeping the three forms distinct
/// is what makes resolution unambiguous: a catalog name is never also treated
/// as a path just because a file with that name happens to exist in `$PWD`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelRef {
    Catalog(&'static ModelInfo),
    Path(PathBuf),
}

/// classify a model string. a bare catalog key (or its `ggml-*.bin` filename)
/// wins; anything that looks like a path is a path. unknown bare names are
/// rejected here so callers can report them instead of guessing.
pub fn classify(input: &str) -> Result<ModelRef, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("empty model name".into());
    }
    let path_like = trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.starts_with('.')
        || trimmed.starts_with('~');
    if !path_like {
        let key = trimmed
            .strip_prefix("ggml-")
            .and_then(|s| s.strip_suffix(".bin"))
            .unwrap_or(trimmed);
        if let Some(info) = find(key) {
            return Ok(ModelRef::Catalog(info));
        }
        // unknown bare name: do not fall back to a path in cwd.
        let suggestion = default_model().key;
        return Err(format!(
            "unknown model '{trimmed}'. use a catalog name (try '{suggestion}') or pass a path to a .bin file"
        ));
    }
    let path = expand_tilde(trimmed);
    if !path.is_file() {
        return Err(format!("no model file at {}", path.display()));
    }
    Ok(ModelRef::Path(path))
}

/// fingerprint used to invalidate cached verification results. if the file
/// changed in any way, the old "verified" stamp is worthless.
type Verified = Mutex<std::collections::HashMap<PathBuf, (u64, SystemTime)>>;
static VERIFIED: OnceLock<Verified> = OnceLock::new();

fn fingerprint(path: &Path) -> Option<(u64, SystemTime)> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

/// validate a catalog model once per version of the file. truncated, corrupted
/// and interrupted downloads are rejected before whisper ever sees them.
pub fn validate_cached(info: &ModelInfo, path: &Path) -> Result<(), String> {
    let stamp = fingerprint(path);
    let cache = VERIFIED.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let (Some(stamp), Ok(map)) = (stamp, cache.lock())
        && map.get(path) == Some(&stamp)
    {
        return Ok(());
    }
    verify_model(info, path)?;
    if let (Some(stamp), Ok(mut map)) = (stamp, cache.lock()) {
        map.insert(path.to_path_buf(), stamp);
    }
    Ok(())
}

/// custom models have no authoritative checksum in the catalog. the least we
/// can do is reject empty stubs and files that cannot possibly be ggml models.
fn read_model_header(path: &Path) -> Result<[u8; 4], String> {
    use std::io::Read;
    let mut file =
        File::open(path).map_err(|e| format!("cannot read model {}: {e}", path.display()))?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)
        .map_err(|e| format!("cannot read model header {}: {e}", path.display()))?;
    Ok(magic)
}

pub fn validate_custom(path: &Path) -> Result<(), String> {
    let len = fs::metadata(path)
        .map_err(|e| format!("cannot inspect {}: {e}", path.display()))?
        .len();
    if len < 1024 {
        return Err(format!(
            "{} is only {} bytes; that is not a model. pass a real ggml .bin file",
            path.display(),
            len
        ));
    }
    let magic = read_model_header(path)?;
    if &magic != b"ggml" && &magic != b"GGUF" {
        return Err(format!(
            "{} does not look like a ggml model (bad header). refusing to hand it to whisper",
            path.display()
        ));
    }
    Ok(())
}

/// Resolve a single model reference to a usable file. `None` means "use the
/// first cached model", which keeps the old no-flags workflow working.
pub fn resolve(input: Option<&str>) -> Result<(String, PathBuf), String> {
    let Some(input) = input else {
        return resolve_cached();
    };
    match classify(input)? {
        ModelRef::Catalog(info) => {
            let path = cache_dir().join(info.file);
            if !path.is_file() {
                return Err(format!(
                    "{} is a valid model but is not downloaded yet ({} MiB). open the model picker with m, or pass --model <path>",
                    info.key, info.size_mib
                ));
            }
            validate_cached(info, &path).map_err(|e| {
                format!("cached {} failed verification: {e}. remove it from the model picker and download it again", info.key)
            })?;
            Ok((info.label.to_string(), path))
        }
        ModelRef::Path(path) => {
            validate_custom(&path)?;
            let label = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "custom".into());
            Ok((label, path))
        }
    }
}

/// First valid cached catalog model, in catalog order. A corrupt file does not
/// block a different, intact model from being used.
pub fn resolve_cached() -> Result<(String, PathBuf), String> {
    let dir = cache_dir();
    let mut corrupt = Vec::new();
    for info in CATALOG {
        let path = dir.join(info.file);
        if !path.is_file() {
            continue;
        }
        match validate_cached(info, &path) {
            Ok(()) => return Ok((info.label.to_string(), path)),
            Err(e) => corrupt.push(format!("{}: {e}", info.key)),
        }
    }
    if corrupt.is_empty() {
        Err(format!(
            "no model cached. open the picker with m and grab {} ({} MiB), or pass --model <path>",
            default_model().label,
            default_model().size_mib
        ))
    } else {
        Err(format!(
            "every cached model failed verification ({}). remove the bad files in the model picker and download again",
            corrupt.join("; ")
        ))
    }
}

#[derive(Clone, Debug)]
pub struct DownloadProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
    pub bytes_per_sec: f64,
}

impl DownloadProgress {
    pub fn fraction(&self) -> f64 {
        match self.total {
            Some(t) if t > 0 => (self.downloaded as f64 / t as f64).clamp(0.0, 1.0),
            _ => 0.0,
        }
    }

    pub fn eta_s(&self) -> Option<f64> {
        if self.bytes_per_sec <= 0.0 {
            return None;
        }
        let total = self.total?;
        let remaining = total.saturating_sub(self.downloaded);
        if remaining == 0 {
            Some(0.0)
        } else {
            Some(remaining as f64 / self.bytes_per_sec)
        }
    }
}

/// anything smaller than 1 MiB is a stub, not a download worth resuming.
/// a range request is the real authority on whether the partial file is valid.
fn resumable_min_bytes() -> u64 {
    1024 * 1024
}

/// if a partial file is this far off, it is garbage. do not bother resuming.
fn min_size(info: &ModelInfo) -> u64 {
    info.size_mib.saturating_mul(1024 * 1024) * 9 / 10
}

/// stream a file through sha256 and compare against the upstream checksum.
/// returns the hex digest so callers can include it in an error message.
fn sha256_of(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("cannot open for checksum: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 256 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("checksum read failed: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Verify a completed model against its catalog size and sha256. Used by the
/// downloader before committing a file, and available for explicit checks.
pub fn verify_model(info: &ModelInfo, path: &Path) -> Result<(), String> {
    let actual = fs::metadata(path)
        .map_err(|e| format!("cannot inspect model {}: {e}", path.display()))?
        .len();
    if actual < min_size(info) {
        return Err(format!(
            "incomplete model: got {} of {} MiB",
            actual / (1024 * 1024),
            info.size_mib
        ));
    }
    let got = sha256_of(path)?;
    if got != info.sha {
        return Err(format!(
            "checksum mismatch for {} (got {}, expected {})",
            info.file,
            &got[..got.len().min(12)],
            &info.sha[..info.sha.len().min(12)]
        ));
    }
    Ok(())
}

pub fn partial_size(info: &ModelInfo) -> Option<u64> {
    let part = cache_dir().join(format!("{}.part", info.file));
    fs::metadata(&part).map(|m| m.len()).ok().filter(|l| *l > 0)
}

pub fn remove_partial(info: &ModelInfo) -> Result<(), String> {
    let dir = cache_dir();
    remove_cached_files(&dir, info.file, [dir.join(format!("{}.part", info.file))])
}

pub fn remove_model(info: &ModelInfo) -> Result<(), String> {
    let dir = cache_dir();
    remove_cached_files(
        &dir,
        info.file,
        [
            dir.join(info.file),
            dir.join(format!("{}.part", info.file)),
            dir.join(format!(".{}.error", info.file)),
        ],
    )
}

fn remove_cached_files(
    dir: &Path,
    file: &str,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<(), String> {
    let _lock = DownloadLock::acquire(dir, file)?;
    for path in paths {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot remove {}: {e}", path.display())),
        }
    }
    Ok(())
}

/// The persistent file serializes current processes. The legacy path uses a
/// directory so older downloaders cannot unlink it as a stale lock file.
#[derive(Debug)]
struct DownloadLock {
    legacy_marker: PathBuf,
    _file: fs::File,
}

impl DownloadLock {
    fn acquire(dir: &Path, file: &str) -> Result<Self, String> {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create model directory: {e}"))?;
        let lock = open_cache_lock(&dir.join(format!(".{file}.cache.lock")))
            .map_err(|e| format!("cannot open model cache lock: {e}"))?;
        if !lock
            .metadata()
            .map_err(|e| format!("cannot inspect model cache lock: {e}"))?
            .is_file()
        {
            return Err("model cache lock is not a regular file".into());
        }
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                return Err(format!(
                    "another scribe is using the model cache for {file}"
                ));
            }
            Err(fs::TryLockError::Error(e)) => return Err(format!("cannot lock model cache: {e}")),
        }

        let legacy_marker = dir.join(format!(".{file}.lock"));
        match fs::create_dir(&legacy_marker) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Only versions that take the OS lock create this directory.
                // If it survived a crash, the OS lock lets us adopt it safely.
                let is_directory =
                    fs::symlink_metadata(&legacy_marker).is_ok_and(|metadata| metadata.is_dir());
                if !is_directory {
                    return Err(format!(
                        "another scribe is using the model cache for {file} (legacy lock at {})",
                        legacy_marker.display()
                    ));
                }
            }
            Err(e) => return Err(format!("cannot create legacy model cache marker: {e}")),
        }
        Ok(Self {
            legacy_marker,
            _file: lock,
        })
    }
}

impl Drop for DownloadLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.legacy_marker);
    }
}

#[cfg(unix)]
fn open_cache_lock(path: &Path) -> std::io::Result<File> {
    use rustix::fs::{Mode, OFlags, open};

    let flags =
        OFlags::RDWR | OFlags::CREATE | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    Ok(open(path, flags, Mode::RUSR | Mode::WUSR).map(File::from)?)
}

#[cfg(windows)]
fn open_cache_lock(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x00200000;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// download to a .part file, verify size and sha256, then rename. the rename is
/// the commit point: until then, nothing in the cache claims to be a model.
pub fn download(
    info: &ModelInfo,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    on_progress: impl Fn(DownloadProgress),
) -> Result<PathBuf, String> {
    download_from(info, BASE_URL, cache_dir(), cancel, on_progress)
}

/// same as `download`, with an injectable base url and cache directory so
/// failure and interrupt paths can be tested against a local server.
fn download_from(
    info: &ModelInfo,
    base_url: &str,
    dir: PathBuf,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    on_progress: impl Fn(DownloadProgress),
) -> Result<PathBuf, String> {
    let _lock = DownloadLock::acquire(&dir, info.file)?;
    let final_path = dir.join(info.file);
    let part = dir.join(format!("{}.part", info.file));
    let url = format!("{}/{}", base_url.trim_end_matches('/'), info.file);
    let mut last_error = String::from("download failed before any bytes were written");

    for _attempt in 0..3 {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("download paused".into());
        }
        let mut existing = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        // only throw away a stub. a user who pauses at 3% of a 3 GiB model has
        // still earned those megabytes, and the range request will tell us if
        // the partial file is no good.
        if existing > 0 && existing < resumable_min_bytes() {
            existing = 0;
        }

        // bounded socket timeouts: a stalled server must not hold cancellation
        // hostage. a read timeout restarts the attempt from the saved partial.
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(std::time::Duration::from_secs(10)))
            .timeout_per_read(Some(std::time::Duration::from_secs(8)))
            .build()
            .new_agent();
        let mut request = agent.get(&url);
        if existing > 0 {
            request = request.header("Range", &format!("bytes={existing}-"));
        }
        let mut response = match request.call() {
            Ok(r) => r,
            Err(e) => {
                last_error = format!("download failed: {e}");
                continue;
            }
        };

        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("download paused".into());
        }

        let resume = response.status() == 206 && existing > 0;
        if existing > 0 && !resume {
            // server said "no" to the range request. start over, no drama.
            existing = 0;
        }
        let total = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .map(|n| n + existing);

        let mut file = if resume {
            match fs::OpenOptions::new().append(true).open(&part) {
                Ok(f) => f,
                Err(_) => {
                    File::create(&part).map_err(|e| format!("cannot write temp file: {e}"))?
                }
            }
        } else {
            File::create(&part).map_err(|e| format!("cannot write temp file: {e}"))?
        };
        if !resume && existing > 0 {
            existing = 0;
        }

        let started = Instant::now();
        let mut last_report = Instant::now();
        let mut buf = [0u8; 64 * 1024];
        let mut downloaded = existing;
        let mut read_error = None;
        {
            let mut reader = response.body_mut().as_reader();
            loop {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = file.flush();
                    return Err("download paused".into());
                }
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if file.write_all(&buf[..n]).is_err() {
                            read_error = Some("cannot write temp file".to_string());
                            break;
                        }
                        downloaded += n as u64;
                        if last_report.elapsed().as_millis() >= 100 {
                            let secs = started.elapsed().as_secs_f64().max(0.001);
                            on_progress(DownloadProgress {
                                downloaded,
                                total,
                                bytes_per_sec: (downloaded - existing) as f64 / secs,
                            });
                            last_report = Instant::now();
                        }
                    }
                    Err(e) => {
                        read_error = Some(format!("read failed: {e}"));
                        break;
                    }
                }
            }
        }
        if let Some(e) = read_error {
            last_error = e;
            continue;
        }
        if file.flush().is_err() {
            last_error = "cannot flush temp file".into();
            continue;
        }
        drop(file);

        let actual = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if let Some(expected) = total
            && actual != expected
        {
            last_error = format!(
                "size mismatch: got {} of {}",
                human_bytes(actual),
                human_bytes(expected)
            );
            let _ = fs::remove_file(&part);
            continue;
        }
        if let Err(e) = verify_model(info, &part) {
            last_error = format!("{e}; the file was removed, retry the download");
            let _ = fs::remove_file(&part);
            continue;
        }
        on_progress(DownloadProgress {
            downloaded: actual,
            total: Some(actual),
            bytes_per_sec: 0.0,
        });
        fs::rename(&part, &final_path).map_err(|e| format!("cannot finalize model file: {e}"))?;
        return Ok(final_path);
    }
    Err(format!("{last_error}. tried 3 times"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_lookup() {
        assert_eq!(find("small-q5_1").unwrap().file, "ggml-small-q5_1.bin");
        assert_eq!(find("large-v3-turbo-q5_0").unwrap().size_mib, 547);
        assert!(find("nope").is_none());
    }

    #[test]
    fn default_is_small_and_multilingual() {
        let m = default_model();
        assert_eq!(m.key, "small-q5_1");
        assert!(m.multilingual);
    }

    #[test]
    fn catalog_is_complete_and_well_formed() {
        assert_eq!(CATALOG.len(), 33);
        let keys: std::collections::HashSet<_> = CATALOG.iter().map(|m| m.key).collect();
        assert_eq!(keys.len(), CATALOG.len(), "duplicate model keys");
        for m in CATALOG {
            assert!(m.sha.len() == 64, "{} has a bad sha", m.key);
            assert!(
                m.sha.bytes().all(|b| b.is_ascii_hexdigit()),
                "{} bad sha",
                m.key
            );
            assert_eq!(m.file, format!("ggml-{}.bin", m.key));
            assert_eq!(
                m.multilingual,
                !m.key.contains(".en"),
                "{} lang flag",
                m.key
            );
        }
        assert!(!find("tiny.en").unwrap().multilingual);
        assert!(find("large-v3").unwrap().multilingual);
    }

    #[test]
    fn progress_math() {
        let p = DownloadProgress {
            downloaded: 50,
            total: Some(100),
            bytes_per_sec: 10.0,
        };
        assert_eq!(p.fraction(), 0.5);
        assert_eq!(p.eta_s(), Some(5.0));
        let q = DownloadProgress {
            downloaded: 5,
            total: None,
            bytes_per_sec: 0.0,
        };
        assert_eq!(q.fraction(), 0.0);
        assert_eq!(q.eta_s(), None);
    }

    #[test]
    fn shas_match_upstream_file_metadata() {
        // spot-check shas against hugging face lfs metadata. the table in the
        // whisper.cpp readme is stale, so these come from the api.
        assert_eq!(
            find("tiny.en").unwrap().sha,
            "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f"
        );
        assert_eq!(
            find("small-q5_1").unwrap().sha,
            "ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb"
        );
        assert_eq!(
            find("large-v3-turbo-q5_0").unwrap().sha,
            "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2"
        );
    }

    #[test]
    fn small_partials_are_worth_resuming() {
        // the old 25%-of-model threshold threw away legitimate pauses on big
        // models. only tiny stubs get discarded now.
        assert_eq!(resumable_min_bytes(), 1024 * 1024);
        let three_percent_of_large = 2969 * 1024 * 1024 * 3 / 100;
        assert!(three_percent_of_large > resumable_min_bytes());
    }

    #[test]
    fn cache_path_uses_xdg() {
        let p = cache_dir();
        assert!(p.ends_with("scribe/models") || p.ends_with("scribe\\models"));
    }

    #[test]
    fn classify_accepts_every_catalog_key() {
        for info in CATALOG {
            assert_eq!(
                classify(info.key).unwrap(),
                ModelRef::Catalog(info),
                "key {}",
                info.key
            );
            assert_eq!(
                classify(info.file).unwrap(),
                ModelRef::Catalog(info),
                "file {}",
                info.file
            );
        }
    }

    #[test]
    fn classify_rejects_unknown_and_empty_names() {
        assert!(classify("not-a-model").is_err());
        assert!(classify("   ").is_err());
        let err = classify("tiny-typo").unwrap_err();
        assert!(err.contains("unknown model") && err.contains("small-q5_1"));
    }

    #[test]
    fn classify_treats_paths_as_paths() {
        let dir = std::env::temp_dir().join(format!("scribe-model-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let file = dir.join("custom-ggml.bin");
        fs::write(&file, b"not a real model").unwrap();
        match classify(file.to_str().unwrap()).unwrap() {
            ModelRef::Path(p) => assert_eq!(p, file),
            other => panic!("expected path, got {other:?}"),
        }
        // a path-like name that does not exist is an error, not a catalog lookup
        assert!(classify("./definitely-missing.bin").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_path_form_returns_label_and_path() {
        let dir = std::env::temp_dir().join(format!("scribe-resolve-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let file = dir.join("my-voice.bin");
        let mut model = b"ggml".to_vec();
        model.resize(2048, 0);
        fs::write(&file, &model).unwrap();
        let (label, path) = resolve(Some(file.to_str().unwrap())).unwrap();
        assert_eq!(label, "my-voice.bin");
        assert_eq!(path, file);
        let _ = fs::remove_dir_all(&dir);
    }

    fn tiny_info(sha: &'static str) -> ModelInfo {
        ModelInfo {
            key: "test",
            file: "ggml-test.bin",
            label: "test",
            size_mib: 0,
            ram_mib: 1,
            speed: "fast",
            note: "test",
            sha,
            multilingual: true,
        }
    }

    /// minimal single-request http server. `mode` controls what it sends so
    /// success, checksum failure and mid-stream interruption are all
    /// deterministic and never touch the public internet.
    fn serve_once(body: &[u8], mode: &str) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let body = body.to_vec();
        let mode = mode.to_string();
        let handle = std::thread::spawn(move || {
            // serve several attempts: the downloader retries up to 3 times.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            for _ in 0..4 {
                let mut stream = loop {
                    if std::time::Instant::now() > deadline {
                        return;
                    }
                    match listener.accept() {
                        Ok((s, _)) => break s,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(_) => return,
                    }
                };
                let mut request = [0u8; 2048];
                let n = stream.read(&mut request).unwrap_or(0);
                let req = String::from_utf8_lossy(&request[..n]);
                match mode.as_str() {
                    "ok" => {
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        stream.write_all(header.as_bytes()).unwrap();
                        stream.write_all(&body).unwrap();
                    }
                    "short" => {
                        // announce a full file but send only half, then disconnect
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        stream.write_all(header.as_bytes()).unwrap();
                        stream.write_all(&body[..body.len() / 2]).unwrap();
                    }
                    "range-ok" => {
                        let start = req
                            .lines()
                            .find_map(|l| l.strip_prefix("Range: bytes="))
                            .and_then(|s| s.trim().split('-').next())
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(0);
                        let payload = &body[start..];
                        let header = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            payload.len()
                        );
                        stream.write_all(header.as_bytes()).unwrap();
                        stream.write_all(payload).unwrap();
                    }
                    _ => {}
                }
                let _ = stream.flush();
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn download_succeeds_and_commits_verified_file() {
        let body = b"hello";
        let info = tiny_info("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        let (url, server) = serve_once(body, "ok");
        let dir = std::env::temp_dir().join(format!("scribe-dl-ok-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let result = download_from(&info, &url, dir.clone(), &cancel, |_| {});
        server.join().unwrap();
        let path = result.unwrap();
        assert_eq!(path, dir.join(info.file));
        assert_eq!(fs::read(&path).unwrap(), body);
        assert!(!dir.join(format!("{}.part", info.file)).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_rejects_checksum_mismatch() {
        let info = tiny_info("0000000000000000000000000000000000000000000000000000000000000000");
        let (url, server) = serve_once(b"hello", "ok");
        let dir = std::env::temp_dir().join(format!("scribe-dl-sha-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // the server only answers once; the three attempts all fail, which is
        // exactly what we want to observe.
        let err = download_from(&info, &url, dir.clone(), &cancel, |_| {}).unwrap_err();
        server.join().unwrap();
        assert!(err.contains("checksum mismatch") || err.contains("download failed"));
        assert!(
            !dir.join(info.file).exists(),
            "bad file must not be committed"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn interrupted_download_is_never_committed() {
        let info = tiny_info("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        let (url, server) = serve_once(b"hello world", "short");
        let dir = std::env::temp_dir().join(format!("scribe-dl-short-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let err = download_from(&info, &url, dir.clone(), &cancel, |_| {}).unwrap_err();
        server.join().unwrap();
        assert!(
            err.contains("size mismatch")
                || err.contains("read failed")
                || err.contains("download failed"),
            "unexpected error: {err}"
        );
        assert!(
            !dir.join(info.file).exists(),
            "truncated file must not be committed"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancelled_download_leaves_no_committed_file() {
        let info = tiny_info("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        let (url, server) = serve_once(b"hello", "ok");
        let dir = std::env::temp_dir().join(format!("scribe-dl-cancel-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let err = download_from(&info, &url, dir.clone(), &cancel, |_| {}).unwrap_err();
        server.join().unwrap();
        assert!(err.contains("paused") || err.contains("cancelled"));
        assert!(!dir.join(info.file).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_model_checks_sha_and_size() {
        let info = ModelInfo {
            key: "test",
            file: "ggml-test.bin",
            label: "test",
            size_mib: 0,
            ram_mib: 1,
            speed: "fast",
            note: "test",
            sha: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            multilingual: true,
        };
        let dir = std::env::temp_dir().join(format!("scribe-verify-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let file = dir.join("ggml-test.bin");
        fs::write(&file, b"hello").unwrap();
        assert!(verify_model(&info, &file).is_ok());
        fs::write(&file, b"world").unwrap();
        let err = verify_model(&info, &file).unwrap_err();
        assert!(err.contains("checksum mismatch"));
        fs::remove_file(&file).unwrap();
        assert!(verify_model(&info, &file).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_download_lock_is_exclusive() {
        let dir = std::env::temp_dir().join(format!(
            "scribe-lock-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let first = DownloadLock::acquire(&dir, "ggml-test.bin").unwrap();
        let second = DownloadLock::acquire(&dir, "ggml-test.bin");
        assert!(second.is_err());
        assert!(second.unwrap_err().contains("another scribe"));
        drop(first);
        assert!(DownloadLock::acquire(&dir, "ggml-test.bin").is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn public_cache_removals_respect_the_download_lock() {
        let dir = std::env::temp_dir().join(format!("scribe-lock-removal-{}", std::process::id()));
        let info = tiny_info("unused");
        let model = dir.join(info.file);
        let partial = dir.join(format!("{}.part", info.file));
        let error = dir.join(format!(".{}.error", info.file));
        TEST_CACHE_DIR.with(|cache| *cache.borrow_mut() = Some(dir.clone()));

        assert!(!dir.exists());
        remove_partial(&info).unwrap();
        assert!(dir.join(format!(".{}.cache.lock", info.file)).is_file());

        fs::write(&model, b"model").unwrap();
        fs::write(&partial, b"partial").unwrap();
        fs::write(&error, b"old note").unwrap();
        let lock = DownloadLock::acquire(&dir, info.file).unwrap();

        assert!(
            remove_partial(&info)
                .unwrap_err()
                .contains("another scribe")
        );
        assert!(remove_model(&info).unwrap_err().contains("another scribe"));
        assert_eq!(fs::read(&model).unwrap(), b"model");
        assert_eq!(fs::read(&partial).unwrap(), b"partial");
        assert_eq!(fs::read(&error).unwrap(), b"old note");

        drop(lock);
        remove_partial(&info).unwrap();
        assert!(!partial.exists());
        assert!(model.exists());
        assert!(error.exists());
        remove_model(&info).unwrap();
        assert!(!model.exists());
        assert!(!error.exists());
        TEST_CACHE_DIR.with(|cache| *cache.borrow_mut() = None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn legacy_downloads_contend_and_cannot_remove_the_marker() {
        let dir = std::env::temp_dir().join(format!("scribe-legacy-lock-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let file = "ggml-test.bin";
        let legacy = dir.join(format!(".{file}.lock"));
        File::create_new(&legacy).unwrap();
        assert!(
            DownloadLock::acquire(&dir, file)
                .unwrap_err()
                .contains("legacy lock")
        );
        fs::remove_file(&legacy).unwrap();

        // A previous new-version process could have exited before dropping the marker.
        fs::create_dir(&legacy).unwrap();
        let lock = DownloadLock::acquire(&dir, file).unwrap();
        assert!(File::create_new(&legacy).is_err());
        assert!(fs::remove_file(&legacy).is_err());
        assert!(legacy.is_dir());
        drop(lock);

        File::create_new(&legacy).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cache_lock_rejects_a_directory() {
        let dir = std::env::temp_dir().join(format!("scribe-nonfile-lock-{}", std::process::id()));
        let file = "ggml-test.bin";
        let lock_path = dir.join(format!(".{file}.cache.lock"));
        fs::create_dir_all(&lock_path).unwrap();
        assert!(DownloadLock::acquire(&dir, file).is_err());
        assert!(lock_path.is_dir());
        assert!(!dir.join(format!(".{file}.lock")).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn custom_model_header_read_errors_are_rejected() {
        let dir = std::env::temp_dir().join(format!("scribe-header-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("truncated.bin");
        fs::write(&path, b"gg").unwrap();
        assert!(read_model_header(&path).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn validate_custom_rejects_stubs_and_wrong_headers() {
        let dir = std::env::temp_dir().join(format!("scribe-custom-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let stub = dir.join("stub.bin");
        fs::write(&stub, b"tiny").unwrap();
        assert!(validate_custom(&stub).unwrap_err().contains("not a model"));
        let fake = dir.join("fake.bin");
        fs::write(&fake, vec![0u8; 2048]).unwrap();
        assert!(
            validate_custom(&fake)
                .unwrap_err()
                .contains("does not look like")
        );
        let good = dir.join("good.bin");
        let mut data = b"ggml".to_vec();
        data.resize(2048, 0);
        fs::write(&good, &data).unwrap();
        assert!(validate_custom(&good).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cached_verification_is_invalidated_when_the_file_changes() {
        let info = tiny_info("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        let dir = std::env::temp_dir().join(format!(
            "scribe-verify-cache-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::create_dir_all(&dir);
        let file = dir.join(info.file);
        fs::write(&file, b"hello").unwrap();
        assert!(validate_cached(&info, &file).is_ok());
        assert!(validate_cached(&info, &file).is_ok());
        fs::write(&file, b"world").unwrap();
        assert!(validate_cached(&info, &file).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_catalog_name_without_download_gives_actionable_error() {
        // use a key that is extremely unlikely to be cached on a test runner
        let key = "large-v1";
        if cache_dir().join(find(key).unwrap().file).is_file() {
            return;
        }
        let err = resolve(Some(key)).unwrap_err();
        assert!(err.contains("not downloaded") && err.contains("2969"));
    }
}
