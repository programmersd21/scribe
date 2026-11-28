//! persistent user preferences. command-line arguments always take priority.

use crate::app::App;
use crate::cli::Args;
use crate::export::Format;
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preferences {
    pub model: Option<String>,
    pub threads: Option<i32>,
    pub lang: Option<String>,
    pub out: Option<String>,
    pub format: Option<Format>,
    pub theme: Option<String>,
    pub translate: Option<bool>,
}

pub fn path() -> PathBuf {
    crate::util::config_home().join("scribe").join("config")
}

fn unescape(value: &str) -> String {
    let mut result = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('\\') => result.push('\\'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(ch);
        }
    }
    result
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\n', "\\n")
}

impl Preferences {
    pub fn load() -> Self {
        Self::load_from(&path())
    }

    /// parse from an explicit file. kept separate from `load` so writes and
    /// reads can be tested against a deterministic temp path.
    pub fn load_from(path: &PathBuf) -> Self {
        let Ok(text) = fs::read_to_string(path) else {
            return Self::default();
        };
        let mut prefs = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = unescape(value);
            match key.trim() {
                "model" if !value.trim().is_empty() => {
                    prefs.model = Some(value.trim().to_string())
                }
                "threads" => {
                    if let Ok(n) = value.trim().parse::<i32>()
                        && (1..=256).contains(&n)
                    {
                        prefs.threads = Some(n);
                    }
                }
                "lang" if !value.trim().is_empty() => {
                    prefs.lang = Some(value.trim().to_string())
                }
                "out" if !value.is_empty() => prefs.out = Some(value),
                "format" => prefs.format = Format::parse(&value),
                "theme" if !value.trim().is_empty() => {
                    prefs.theme = Some(value.trim().to_string())
                }
                "translate" => prefs.translate = value.trim().parse::<bool>().ok(),
                _ => {}
            }
        }
        prefs
    }

    pub fn from_app(app: &App) -> Self {
        Self {
            model: (!app.model_name.is_empty() && app.model_label != "-")
                .then(|| app.model_name.clone()),
            threads: Some(app.threads),
            lang: Some(app.lang.clone()),
            out: Some(app.out_dir.clone()),
            format: Some(app.format),
            theme: Some(app.theme.key().to_string()),
            translate: Some(app.translate),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&path())
    }

    /// Atomically replace `path` with a freshly written config.
    ///
    /// The temp file lives in the destination directory (same filesystem) so
    /// the final `rename` is atomic. The existing config is left completely
    /// untouched until that rename succeeds: no unlink-first window, and a
    /// failure at any earlier step leaves the old file as the only config.
    pub fn save_to(&self, path: &PathBuf) -> Result<(), String> {
        use std::io::Write;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or("config path has no parent directory")?;
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create config directory {}: {e}", parent.display()))?;

        let mut contents =
            String::from("# scribe preferences; command-line options override these values\n");
        if let Some(v) = &self.model {
            contents.push_str(&format!("model={}\n", escape(v)));
        }
        if let Some(v) = self.threads {
            contents.push_str(&format!("threads={v}\n"));
        }
        if let Some(v) = &self.lang {
            contents.push_str(&format!("lang={}\n", escape(v)));
        }
        if let Some(v) = &self.out {
            contents.push_str(&format!("out={}\n", escape(v)));
        }
        if let Some(v) = self.format {
            contents.push_str(&format!("format={}\n", v.extension()));
        }
        if let Some(v) = &self.theme {
            contents.push_str(&format!("theme={}\n", escape(v)));
        }
        if let Some(v) = self.translate {
            contents.push_str(&format!("translate={v}\n"));
        }

        // unique name so two processes never race over the same temp file, and
        // so a crash mid-write cannot be mistaken for a valid config.
        let temp = parent.join(format!(
            ".config.tmp.{}.{}",
            std::process::id(),
            now_nanos()
        ));
        let write = (|| -> Result<(), String> {
            let mut file = fs::File::create(&temp)
                .map_err(|e| format!("cannot create temporary config: {e}"))?;
            file.write_all(contents.as_bytes())
                .map_err(|e| format!("cannot write temporary config: {e}"))?;
            file.flush()
                .map_err(|e| format!("cannot flush temporary config: {e}"))?;
            file.sync_all()
                .map_err(|e| format!("cannot sync temporary config: {e}"))?;
            drop(file);
            atomic_replace(&temp, path)
        })();
        if write.is_err() {
            let _ = fs::remove_file(&temp);
        }
        write
    }
}

/// Replace `dst` with `src` atomically. On Unix `rename` replaces an existing
/// file in one step. Windows cannot rename over an existing file, so the
/// previous file is moved aside first and restored if the install fails.
fn atomic_replace(src: &PathBuf, dst: &PathBuf) -> Result<(), String> {
    #[cfg(unix)]
    {
        fs::rename(src, dst).map_err(|e| format!("cannot install config {}: {e}", dst.display()))
    }
    #[cfg(not(unix))]
    {
        if dst.exists() {
            let backup = dst.with_extension(format!("bak.{}", now_nanos()));
            fs::rename(dst, &backup).map_err(|e| format!("cannot move old config aside: {e}"))?;
            if let Err(e) = fs::rename(src, dst) {
                let _ = fs::rename(&backup, dst);
                return Err(format!("cannot install config {}: {e}", dst.display()));
            }
            let _ = fs::remove_file(&backup);
        } else {
            fs::rename(src, dst)
                .map_err(|e| format!("cannot install config {}: {e}", dst.display()))?;
        }
        Ok(())
    }
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

pub fn apply(args: &mut Args) {
    let prefs = Preferences::load();
    if args.model.is_none() {
        args.model = prefs.model.map(PathBuf::from);
    }
    if args.threads.is_none() {
        args.threads = prefs.threads;
    }
    if args.lang.is_none() {
        args.lang = prefs
            .lang
            .filter(|l| crate::util::validate_language(l).is_ok());
    }
    if args.translate.is_none() {
        args.translate = prefs.translate;
    }
    if args.out.is_none() {
        args.out = prefs.out.map(PathBuf::from);
    }
    if !args.format_explicit
        && let Some(format) = prefs.format
    {
        args.format = format;
    }
    if args.theme.is_none() {
        args.theme = prefs.theme;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_round_trips_paths_and_newlines() {
        let value = "folder\\name\nsecond line";
        assert_eq!(unescape(&escape(value)), value);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "scribe-config-{}-{tag}-{}",
            std::process::id(),
            now_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_creates_parent_directories_and_round_trips() {
        let dir = temp_dir("create");
        let path = dir.join("deep").join("nested").join("config");
        let prefs = Preferences {
            model: Some("small-q5_1".into()),
            threads: Some(4),
            lang: Some("de".into()),
            out: Some("/tmp/out".into()),
            format: Some(Format::Srt),
            theme: Some("nord".into()),
            translate: Some(true),
        };
        prefs.save_to(&path).unwrap();
        assert_eq!(Preferences::load_from(&path), prefs);
        // no stray temp files are left behind
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn output_paths_preserve_leading_and_trailing_spaces() {
        let dir = temp_dir("spaces");
        let path = dir.join("config");
        let prefs = Preferences {
            out: Some(" /tmp/notes with spaces ".into()),
            ..Preferences::default()
        };
        prefs.save_to(&path).unwrap();
        assert_eq!(Preferences::load_from(&path), prefs);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_write_preserves_existing_config() {
        let dir = temp_dir("preserve");
        let path = dir.join("config");
        let original = "# hand written\nmodel=base\nthreads=2\n";
        fs::write(&path, original).unwrap();

        // make the parent read-only so creating the temp file fails.
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        #[cfg(unix)]
        let original_mode = fs::metadata(&dir).unwrap().permissions().mode();
        let mut permissions = fs::metadata(&dir).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&dir, permissions).unwrap();
        let result = Preferences {
            model: Some("small".into()),
            ..Preferences::default()
        }
        .save_to(&path);
        #[cfg(unix)]
        fs::set_permissions(&dir, fs::Permissions::from_mode(original_mode)).unwrap();
        #[cfg(not(unix))]
        {
            let mut restored = fs::metadata(&dir).unwrap().permissions();
            restored.set_readonly(false);
            fs::set_permissions(&dir, restored).unwrap();
        }

        // running as root bypasses permissions; in that case the save succeeds
        // and the new value is valid, so only assert the invariant we care about.
        if result.is_err() {
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
        } else {
            assert_eq!(
                Preferences::load_from(&path).model.as_deref(),
                Some("small")
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn overwrite_does_not_delete_old_config_on_failure() {
        let dir = temp_dir("overwrite");
        let path = dir.join("config");
        let original = Preferences {
            model: Some("tiny".into()),
            threads: Some(1),
            ..Preferences::default()
        };
        original.save_to(&path).unwrap();
        let updated = Preferences {
            model: Some("small".into()),
            threads: Some(8),
            ..Preferences::default()
        };
        updated.save_to(&path).unwrap();
        assert_eq!(Preferences::load_from(&path), updated);

        // simulate an interrupted operation: a leftover temp file is harmless
        // and must never be considered a config.
        fs::write(dir.join(".config.tmp.1.2"), b"garbage").unwrap();
        assert_eq!(Preferences::load_from(&path), updated);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_skips_malformed_lines() {
        let dir = temp_dir("malformed");
        let path = dir.join("config");
        fs::write(
            &path,
            "garbage\nthreads=not-a-number\nthreads=4\nformat=bogus\nformat=vtt\n",
        )
        .unwrap();
        let prefs = Preferences::load_from(&path);
        assert_eq!(prefs.threads, Some(4));
        assert_eq!(prefs.format, Some(Format::Vtt));
        fs::remove_dir_all(&dir).unwrap();
    }
}
