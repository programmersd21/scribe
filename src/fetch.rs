//! url ingestion. the yt-dlp crate is async, so we spin up a tiny current-
//! thread tokio runtime inside a plain std thread and keep the rest of scribe
//! refreshingly synchronous.
//!
//! cancellation is wired through every phase: installation, metadata fetch and
//! both download paths all race against a cancellation watcher, and any child
//! process we spawn ourselves is killed and reaped before returning.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::process::Child;

pub struct FetchedAudio {
    pub path: PathBuf,
    pub title: String,
    pub duration: Option<f64>,
}

/// where the managed binaries live. versioned directory so a future yt-dlp
/// upgrade cannot trip over yesterday's binary and make a mess of things.
pub fn libs_dir() -> PathBuf {
    crate::util::cache_home()
        .join("scribe")
        .join("bin")
        .join("yt-dlp-2")
}

fn output_dir() -> PathBuf {
    libs_dir().join("audio")
}

/// fetch audio and its metadata, observing `cancel` throughout.
pub fn fetch(url: &str, cancel: &Arc<AtomicBool>) -> Result<FetchedAudio, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("could not start the network runtime: {e}"))?;
    rt.block_on(async move { fetch_async(url, cancel).await })
}

/// Poll the cancellation flag quickly. `tokio::select!` drops the losing
/// future, so this gives every network phase a ~25 ms cancellation latency
/// even when the underlying library offers no cancellation hook of its own.
async fn cancelled(cancel: &Arc<AtomicBool>) {
    while !cancel.load(Ordering::Relaxed) {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

macro_rules! cancellable {
    ($cancel:expr, $future:expr, $what:expr) => {
        tokio::select! {
            result = $future => result.map_err(|e| format!("{}: {e}", $what))?,
            _ = cancelled($cancel) => return Err("cancelled".into()),
        }
    };
}

/// Kill and reap a child we spawned, with a short grace period. No orphans,
/// no zombies on any exit path.
async fn stop_child(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn fetch_async(url: &str, cancel: &Arc<AtomicBool>) -> Result<FetchedAudio, String> {
    use yt_dlp::Downloader;
    use yt_dlp::client::deps::{Libraries, LibraryInstaller};

    let dir = libs_dir();
    let out = output_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create tools directory: {e}"))?;
    std::fs::create_dir_all(&out).map_err(|e| format!("cannot create audio directory: {e}"))?;

    let ytdlp = dir.join(if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    });
    let ffmpeg = dir.join(if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    });

    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }

    // install what is missing. the crate knows which release asset belongs to
    // which os/arch, which is one less lookup table i have to maintain.
    if !ytdlp.is_file() || !ffmpeg.is_file() {
        let installer = LibraryInstaller::new(dir.clone());
        if !ytdlp.is_file() {
            let installed = cancellable!(
                cancel,
                installer.install_youtube(None),
                "could not install yt-dlp"
            );
            if installed != ytdlp {
                std::fs::copy(&installed, &ytdlp)
                    .map_err(|e| format!("could not place yt-dlp binary: {e}"))?;
            }
        }
        if !ffmpeg.is_file() {
            let installed = cancellable!(
                cancel,
                installer.install_ffmpeg(None),
                "could not install ffmpeg"
            );
            if installed != ffmpeg {
                std::fs::copy(&installed, &ffmpeg)
                    .map_err(|e| format!("could not place ffmpeg binary: {e}"))?;
            }
        }
    }

    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }

    let libs = Libraries::new(ytdlp.clone(), ffmpeg.clone());
    let downloader = cancellable!(
        cancel,
        Downloader::builder(libs, out.clone()).build(),
        "could not start the downloader"
    );

    let video = cancellable!(
        cancel,
        downloader.fetch_video_infos(url),
        "could not read that url"
    );
    let title = if video.title.trim().is_empty() {
        "web audio".to_string()
    } else {
        video.title.clone()
    };
    let duration = video
        .duration
        .and_then(|d| if d > 0 { Some(d as f64) } else { None });

    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }

    // unique name so two runs cannot fight over the same file.
    let name = format!("scribe-{}-{}.m4a", std::process::id(), now_millis());
    let direct = out.join(&name);
    let crate_result = tokio::select! {
        result = downloader.download_audio_stream(&video, &name) => result,
        _ = cancelled(cancel) => {
            let _ = std::fs::remove_file(&direct);
            return Err("cancelled".into());
        }
    };
    let path = match crate_result {
        Ok(p) if p.is_absolute() => p,
        Ok(p) => out.join(p),
        Err(crate_err) => {
            // youtube's cdn sometimes says 403 to direct http downloads. fall
            // back to the managed yt-dlp binary, which knows about player
            // clients and signatures. this child is explicitly tracked so a
            // cancel during the fallback kills and reaps it.
            let fallback = out.join(format!(
                "scribe-{}-{}.webm",
                std::process::id(),
                now_millis()
            ));
            let mut child = tokio::process::Command::new(&ytdlp)
                .args(["--no-playlist", "--no-warnings", "-f", "bestaudio/best", "-o"])
                .arg(&fallback)
                .arg(url)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| format!("could not download audio: {e}"))?;
            let status = tokio::select! {
                s = child.wait() => s.map_err(|e| format!("could not download audio: {e}"))?,
                _ = cancelled(cancel) => {
                    stop_child(&mut child).await;
                    let _ = std::fs::remove_file(&fallback);
                    return Err("cancelled".into());
                }
            };
            let _ = child.wait().await;
            if !status.success() || !fallback.is_file() {
                let _ = std::fs::remove_file(&fallback);
                let _ = std::fs::remove_file(&direct);
                return Err(format!(
                    "no audio came out. the http path said: {crate_err}"
                ));
            }
            fallback
        }
    };

    if cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&direct);
        return Err("cancelled".into());
    }
    // the crate may have written to `direct` even when it reported another
    // path; keep only the file we actually return.
    if direct != path {
        let _ = std::fs::remove_file(&direct);
    }

    Ok(FetchedAudio {
        path,
        title,
        duration,
    })
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// have the managed tools been installed yet? used for the startup hint.
pub fn tools_ready() -> bool {
    let dir = libs_dir();
    dir.join(if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    })
    .is_file()
        && dir
            .join(if cfg!(windows) {
                "ffmpeg.exe"
            } else {
                "ffmpeg"
            })
            .is_file()
}

/// path to the managed ffmpeg binary, or none until it is installed.
pub fn ffmpeg_path() -> Option<PathBuf> {
    let p = libs_dir().join(if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    });
    p.is_file().then_some(p)
}

/// remove a partial audio file left behind by a cancelled/interrupted fetch.
pub fn cleanup_audio(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_under_cache() {
        let d = libs_dir();
        assert!(d.to_string_lossy().contains("scribe"));
        assert!(d.ends_with("yt-dlp-2"));
    }

    #[test]
    fn ffmpeg_lookup_is_safe() {
        let _ = ffmpeg_path();
        let _ = tools_ready();
    }

    #[test]
    fn cancellation_is_observed_without_network() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let cancel = Arc::new(AtomicBool::new(true));
        let start = std::time::Instant::now();
        let result = rt.block_on(async {
            tokio::select! {
                _ = cancelled(&cancel) => Err::<(), _>("cancelled"),
                _ = std::future::pending::<()>() => Ok(()),
            }
        });
        assert!(result.is_err());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "cancellation took too long"
        );
    }

    #[test]
    fn cleanup_removes_partial_audio() {
        let dir = std::env::temp_dir().join(format!("scribe-fetch-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("partial.m4a");
        std::fs::write(&file, b"x").unwrap();
        cleanup_audio(&file);
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
