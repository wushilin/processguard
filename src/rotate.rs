//! Size-based rotation of the app's stdout/stderr files.
//!
//! The app holds the live file open (O_APPEND), so it cannot be renamed away;
//! it is copied to `<file>.1` and truncated in place. Older generations shift
//! up (`.1` -> `.2` ...), those past `compress_after` are gzipped, and those
//! past `max_keep` are deleted.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;

pub struct Policy {
    pub max_size: u64,
    pub max_keep: u32,
    pub compress_after: u32,
    pub compression: CompressionMethod,
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

/// Copies the live file to `dest`, then truncates it in place. Writes landing
/// between the final read and the truncate are lost; the tail pass keeps that
/// window as small as possible.
fn copy_truncate(live: &Path, dest: &Path) -> io::Result<()> {
    let mut src = File::open(live)?;
    let truncate = OpenOptions::new().write(true).open(live)?;
    let mut dst = File::create(dest)?;
    io::copy(&mut src, &mut dst)?;
    dst.sync_all()?;
    io::copy(&mut src, &mut dst)?;
    truncate.set_len(0)
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

/// Rotates `live` if it has reached `max_size`. Returns the rotated size.
///
/// Compression can take longer than a cron interval; the caller must hold the
/// rotation lock, and the live file is allowed to outgrow `max_size` meanwhile.
pub fn rotate(live: &Path, policy: &Policy) -> io::Result<Option<u64>> {
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
        return Ok(Some(size));
    }

    // Shift by rename only, so the live file is truncated as soon as possible.
    remove(&generation(policy.max_keep))?;
    remove(&compressed(policy.max_keep))?;
    for n in (1..policy.max_keep).rev() {
        rename(&generation(n), &generation(n + 1))?;
        rename(&compressed(n), &compressed(n + 1))?;
    }
    copy_truncate(live, &generation(1))?;

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
    Ok(Some(size))
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

        assert_eq!(rotate(&live, &policy).expect("rotate"), Some(6));
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

        assert_eq!(rotate(&live, &policy).expect("rotate"), Some(6));
        assert!(!test_dir.0.join("stdout.log.1").exists());
        assert!(test_dir.0.join("stdout.log.1.gz").exists());
    }
}
