//! Size-based rotation of the app's stdout/stderr files.
//!
//! The app holds the live file open (O_APPEND), so it cannot be renamed away;
//! it is copied to `<file>.1` and truncated in place, optionally with the app
//! suspended for the final step so nothing is lost. Older generations shift
//! up (`.1` -> `.2` ...), those past `compress_after` are gzipped, and those
//! past `max_keep` are deleted.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::time::Duration;

use flate2::Compression;
use flate2::write::GzEncoder;

use crate::suspend::Suspend;

pub struct Policy {
    pub max_size: u64,
    pub max_keep: u32,
    pub compress_after: u32,
    pub compression: CompressionMethod,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RotationMethod {
    /// Copy, then truncate. Output written in the instant between the two is lost.
    #[default]
    CopyTruncate,
    /// As above, with the app's process group stopped across the final copy
    /// and the truncate, so nothing is lost.
    Suspend,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Rotated {
    pub size: u64,
    pub truncation: Truncation,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Truncation {
    /// No suspension was requested (or nothing is kept, so nothing can be lost).
    Plain,
    /// The app was suspended for this long.
    Suspended(Duration),
    /// Suspension was requested but the group could not be confirmed stopped.
    SuspendFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompressionMethod {
    Gzip,
    None,
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn rename(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Copies the live file to `dest`, then truncates it in place.
///
/// Without `suspend`, writes landing between the final read and the truncate
/// are lost; the tail pass keeps that window as small as possible. With it,
/// the writers are stopped for the tail pass and the truncate. The bulk copy
/// and the fsync, which are slow, happen while the app still runs.
fn copy_truncate(live: &Path, dest: &Path, suspend: Option<&Suspend>) -> io::Result<Truncation> {
    let mut src = File::open(live)?;
    let truncate = OpenOptions::new().write(true).open(live)?;
    let mut dst = File::create(dest)?;
    io::copy(&mut src, &mut dst)?;
    dst.sync_all()?;

    let frozen = match suspend {
        Some(suspend) => suspend.freeze()?,
        None => None,
    };
    // Twice: a write that was already inside the kernel when its thread was
    // told to stop completes right after the first pass.
    io::copy(&mut src, &mut dst)?;
    io::copy(&mut src, &mut dst)?;
    truncate.set_len(0)?;
    let truncation = match (&frozen, suspend) {
        (Some(frozen), _) => Truncation::Suspended(frozen.elapsed()),
        (None, Some(_)) => Truncation::SuspendFailed,
        (None, None) => Truncation::Plain,
    };
    drop(frozen);
    dst.sync_all()?;
    Ok(truncation)
}

fn gzip(src: &Path, dest: &Path) -> io::Result<()> {
    let tmp = suffixed(dest, ".tmp");
    let mut input = File::open(src)?;
    let mut encoder = GzEncoder::new(BufWriter::new(File::create(&tmp)?), Compression::default());
    io::copy(&mut input, &mut encoder)?;
    encoder.finish()?.into_inner()?.sync_all()?;
    fs::rename(&tmp, dest)?;
    fs::remove_file(src)
}

/// Rotates `live` if it has reached `max_size`. `suspend` names the writers to
/// stop while truncating, for lossless rotation.
///
/// Compression can take longer than a cron interval; the caller must hold the
/// rotation lock, and the live file is allowed to outgrow `max_size` meanwhile.
pub fn rotate(
    live: &Path,
    policy: &Policy,
    suspend: Option<&Suspend>,
) -> io::Result<Option<Rotated>> {
    let generation = |n: u32| suffixed(live, &format!(".{n}"));
    let compressed = |n: u32| suffixed(live, &format!(".{n}.gz"));

    // Finish compression left over from an interrupted run.
    if policy.compression == CompressionMethod::Gzip
        && let Some(first_compressed) = policy.compress_after.checked_add(1)
    {
        for n in first_compressed..=policy.max_keep {
            if generation(n).exists() {
                gzip(&generation(n), &compressed(n))?;
            }
        }
    }

    let size = match fs::metadata(live) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if size < policy.max_size {
        return Ok(None);
    }

    if policy.max_keep == 0 {
        OpenOptions::new().write(true).open(live)?.set_len(0)?;
        return Ok(Some(Rotated {
            size,
            truncation: Truncation::Plain,
        }));
    }

    // Shift by rename only, so the live file is truncated as soon as possible.
    remove(&generation(policy.max_keep))?;
    remove(&compressed(policy.max_keep))?;
    for n in (1..policy.max_keep).rev() {
        rename(&generation(n), &generation(n + 1))?;
        rename(&compressed(n), &compressed(n + 1))?;
    }
    let truncation = copy_truncate(live, &generation(1), suspend)?;

    // The slow part, done last: the generation that just crossed compress_after.
    if policy.compression == CompressionMethod::Gzip
        && let Some(first_compressed) = policy.compress_after.checked_add(1)
    {
        for n in first_compressed..=policy.max_keep {
            if generation(n).exists() {
                gzip(&generation(n), &compressed(n))?;
            }
        }
    }
    Ok(Some(Rotated { size, truncation }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "processguard-rotate-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn compression_none_keeps_plain_generations() {
        let test_dir = TestDir::new();
        let live = test_dir.0.join("stdout.log");
        fs::write(&live, b"output").expect("write live log");
        let policy = Policy {
            max_size: 1,
            max_keep: 2,
            compress_after: 0,
            compression: CompressionMethod::None,
        };

        let rotated = rotate(&live, &policy, None).expect("rotate");
        assert_eq!(
            rotated,
            Some(Rotated {
                size: 6,
                truncation: Truncation::Plain
            })
        );
        assert!(test_dir.0.join("stdout.log.1").exists());
        assert!(!test_dir.0.join("stdout.log.1.gz").exists());
    }

    #[test]
    fn gzip_compresses_generations_after_threshold() {
        let test_dir = TestDir::new();
        let live = test_dir.0.join("stdout.log");
        fs::write(&live, b"output").expect("write live log");
        let policy = Policy {
            max_size: 1,
            max_keep: 2,
            compress_after: 0,
            compression: CompressionMethod::Gzip,
        };

        let rotated = rotate(&live, &policy, None).expect("rotate");
        assert_eq!(
            rotated,
            Some(Rotated {
                size: 6,
                truncation: Truncation::Plain
            })
        );
        assert!(!test_dir.0.join("stdout.log.1").exists());
        assert!(test_dir.0.join("stdout.log.1.gz").exists());
    }
}
