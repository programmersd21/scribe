//! read-only readiness check. no installs, no writes, no surprises.

use crate::{config, fetch, model, util, worker};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    Warn,
    Fail,
}

impl Verdict {
    fn label(self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Warn => "warn",
            Verdict::Fail => "fail",
        }
    }
}

struct Row {
    label: &'static str,
    verdict: Verdict,
    detail: String,
}

fn cache_status(cache: &std::path::Path) -> (Verdict, &'static str) {
    if cache.is_dir() {
        return (Verdict::Ok, "directory exists; write access not tested");
    }
    if std::fs::symlink_metadata(cache).is_ok() {
        return (Verdict::Fail, "path exists but is not a usable directory");
    }
    for ancestor in cache.ancestors().skip(1) {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.is_dir() || ancestor.is_dir() => {
                return (
                    Verdict::Warn,
                    "will be created on first download; write access not tested",
                );
            }
            Ok(_) => return (Verdict::Fail, "an ancestor is not a directory"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return (Verdict::Fail, "cannot inspect cache path"),
        }
    }
    (Verdict::Fail, "no existing parent directory")
}

fn version(binary: &std::path::Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(binary)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .next()
        .map(|line| line.trim().chars().take(80).collect())
}

pub fn run(selected_model: Option<&str>) -> i32 {
    let mut rows = Vec::new();
    rows.push(Row {
        label: "scribe",
        verdict: Verdict::Ok,
        detail: env!("CARGO_PKG_VERSION").into(),
    });
    rows.push(Row {
        label: "os / arch",
        verdict: Verdict::Ok,
        detail: format!("{} / {}", std::env::consts::OS, std::env::consts::ARCH),
    });

    match worker::resolve_ffmpeg() {
        Ok(path) => {
            let found = version(&path, &["-version"]);
            let verdict = if found.is_some() {
                Verdict::Ok
            } else {
                Verdict::Fail
            };
            let detail = found
                .map(|v| format!("{} · {}", path.display(), v))
                .unwrap_or_else(|| format!("{} (self check failed)", path.display()));
            rows.push(Row {
                label: "ffmpeg (required)",
                verdict,
                detail,
            });
        }
        Err(e) => rows.push(Row {
            label: "ffmpeg (required)",
            verdict: Verdict::Fail,
            detail: e,
        }),
    }

    let managed = fetch::libs_dir();
    let ytdlp = managed.join(if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    });
    if ytdlp.is_file() {
        let detail =
            version(&ytdlp, &["--version"]).unwrap_or_else(|| "installed, version unknown".into());
        rows.push(Row {
            label: "yt-dlp (url only)",
            verdict: Verdict::Ok,
            detail,
        });
    } else {
        rows.push(Row {
            label: "yt-dlp (url only)",
            verdict: Verdict::Warn,
            detail: "not installed; scribe fetches it on first url use".into(),
        });
    }
    rows.push(Row {
        label: "managed tools",
        verdict: if fetch::tools_ready() {
            Verdict::Ok
        } else {
            Verdict::Warn
        },
        detail: managed.display().to_string(),
    });

    let cache = model::cache_dir();
    let (cache_verdict, cache_detail) = cache_status(&cache);
    rows.push(Row {
        label: "model cache",
        verdict: cache_verdict,
        detail: format!("{} ({cache_detail})", cache.display()),
    });

    let chosen = selected_model.map(str::to_string).or_else(|| {
        model::resolve_cached()
            .ok()
            .map(|(_, path)| path.display().to_string())
    });
    match chosen {
        Some(name) => {
            let catalog_hit = model::CATALOG
                .iter()
                .find(|info| name == info.file || name.ends_with(&format!("/{}", info.file)));
            if let Some(info) = catalog_hit {
                let path = cache.join(info.file);
                let verdict = if !path.is_file() {
                    Verdict::Warn
                } else if model::validate_cached(info, &path).is_ok() {
                    Verdict::Ok
                } else {
                    Verdict::Fail
                };
                let detail = if !path.is_file() {
                    format!("{} not downloaded ({} MiB)", info.key, info.size_mib)
                } else {
                    format!("{} · {}", info.label, verdict.label())
                };
                rows.push(Row {
                    label: "selected model",
                    verdict,
                    detail,
                });
            } else {
                match model::classify(&name) {
                    Ok(model::ModelRef::Catalog(info)) => {
                        let path = cache.join(info.file);
                        if path.is_file() {
                            match model::validate_cached(info, &path) {
                                Ok(()) => rows.push(Row {
                                    label: "selected model",
                                    verdict: Verdict::Ok,
                                    detail: format!("{} · verified", info.label),
                                }),
                                Err(e) => rows.push(Row {
                                    label: "selected model",
                                    verdict: Verdict::Fail,
                                    detail: e,
                                }),
                            }
                        } else {
                            rows.push(Row {
                                label: "selected model",
                                verdict: Verdict::Warn,
                                detail: format!(
                                    "{} not downloaded ({} MiB)",
                                    info.key, info.size_mib
                                ),
                            });
                        }
                    }
                    Ok(model::ModelRef::Path(path)) => match model::validate_custom(&path) {
                        Ok(()) => rows.push(Row {
                            label: "selected model",
                            verdict: Verdict::Ok,
                            detail: format!("{} · header ok", path.display()),
                        }),
                        Err(e) => rows.push(Row {
                            label: "selected model",
                            verdict: Verdict::Fail,
                            detail: e,
                        }),
                    },
                    Err(e) => rows.push(Row {
                        label: "selected model",
                        verdict: Verdict::Fail,
                        detail: e,
                    }),
                }
            }
        }
        None => rows.push(Row {
            label: "selected model",
            verdict: Verdict::Warn,
            detail: format!(
                "none cached; open the picker with m and grab {}",
                model::default_model().label
            ),
        }),
    }

    let cfg = config::path();
    let readable = std::fs::read_to_string(&cfg).is_ok();
    rows.push(Row {
        label: "config",
        verdict: if readable || !cfg.exists() {
            Verdict::Ok
        } else {
            Verdict::Warn
        },
        detail: format!(
            "{} ({})",
            cfg.display(),
            if readable {
                "readable"
            } else if cfg.exists() {
                "unreadable"
            } else {
                "not created yet"
            }
        ),
    });
    rows.push(Row {
        label: "user dirs",
        verdict: Verdict::Ok,
        detail: format!(
            "cache base {} · config base {}",
            util::cache_home().display(),
            util::config_home().display()
        ),
    });

    let width = rows
        .iter()
        .map(|r| r.label.len())
        .max()
        .unwrap_or(0)
        .max(18);
    println!(
        "scribe doctor — {} {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    );
    println!();
    for row in &rows {
        println!(
            "{:<width$}  {:<4}  {}",
            row.label,
            row.verdict.label(),
            row.detail,
            width = width
        );
    }
    let failures = rows.iter().filter(|r| r.verdict == Verdict::Fail).count();
    if failures == 0 {
        println!("\nready. missing url tools are fetched on first use.");
        0
    } else {
        println!("\n{failures} problem(s) need attention before transcription will work.");
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_status_does_not_claim_unchecked_write_access() {
        let dir = std::env::temp_dir().join(format!("scribe-doctor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache = dir.join("models");
        assert_eq!(
            cache_status(&cache),
            (
                Verdict::Warn,
                "will be created on first download; write access not tested"
            )
        );
        std::fs::create_dir_all(&cache).unwrap();
        assert_eq!(
            cache_status(&cache),
            (Verdict::Ok, "directory exists; write access not tested")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cache_status_rejects_a_file_at_the_cache_path() {
        let dir = std::env::temp_dir().join(format!("scribe-doctor-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cache = dir.join("models");
        std::fs::write(&cache, b"not a directory").unwrap();
        assert_eq!(
            cache_status(&cache),
            (Verdict::Fail, "path exists but is not a directory")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cache_status_rejects_dangling_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("scribe-doctor-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cache = dir.join("models");
        symlink(dir.join("missing"), &cache).unwrap();
        assert_eq!(
            cache_status(&cache),
            (Verdict::Fail, "path exists but is not a usable directory")
        );
        std::fs::remove_file(&cache).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cache_status_accepts_symlinked_directory_ancestors() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("scribe-doctor-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("target")).unwrap();
        symlink(dir.join("target"), dir.join("linked")).unwrap();
        let cache = dir.join("linked").join("nested").join("models");
        assert_eq!(
            cache_status(&cache),
            (
                Verdict::Warn,
                "will be created on first download; write access not tested"
            )
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cache_status_accepts_missing_parent_trees() {
        let dir = std::env::temp_dir().join(format!("scribe-doctor-nested-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!dir.exists());
        let cache = dir.join("one").join("two").join("models");
        assert_eq!(
            cache_status(&cache),
            (
                Verdict::Warn,
                "will be created on first download; write access not tested"
            )
        );
    }

    #[test]
    fn verdict_labels_are_short() {
        assert_eq!(Verdict::Ok.label(), "ok");
        assert_eq!(Verdict::Warn.label(), "warn");
        assert_eq!(Verdict::Fail.label(), "fail");
    }
}
