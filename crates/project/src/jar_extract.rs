//! claude-review-v2 fork: open files inside JAR archives transparently.
//!
//! Java/Kotlin language servers (kotlin-lsp, jdtls, …) sometimes return
//! locations inside a `.jar` source archive using the JVM convention:
//!
//!   `/path/to/foo-1.2.3-sources.jar!/com/example/Foo.kt`
//!
//! Zed can't open that path directly — there's no real file at the
//! virtual `<jar>!/<entry>` URL. This module:
//!
//! 1. Detects the `<archive>.jar!/<entry>` shape.
//! 2. Extracts the requested entry once into a per-jar cache under Zed's
//!    data dir (`<data_dir>/jar-extracts/<hash>/<entry>`).
//! 3. Marks the cached file mode 0o444 so users don't accidentally edit
//!    a stale dependency source.
//!
//! Callers (currently `LspStore::open_local_buffer_via_lsp`) treat the
//! cached path as if it were the original location: Zed opens it with
//! the standard buffer flow, syntax highlighting and tree-sitter all
//! "just work" because it's a real file on disk in a known layout.

use anyhow::{Context as _, Result, anyhow};
use async_zip::base::read::seek::ZipFileReader;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Pull the jar archive path and the in-archive entry path out of a
/// possibly-jar-embedded path string. Returns `None` for ordinary file
/// paths so callers can use this as a cheap branch.
///
/// The convention is `<absolute-path-to-archive>.jar!/<entry-relative>`
/// where the entry path uses forward slashes regardless of OS. We use
/// `rfind` so a path with `!` characters in directory names still works
/// as long as the archive itself ends in `.jar`.
pub fn parse_jar_path(path: &Path) -> Option<(PathBuf, String)> {
    let s = path.to_str()?;
    let idx = s.rfind(".jar!/")?;
    let jar_end = idx + ".jar".len();
    let jar = PathBuf::from(&s[..jar_end]);
    let entry = &s[jar_end + 2..]; // skip the literal "!/"
    if entry.is_empty() {
        return None;
    }
    Some((jar, entry.to_string()))
}

/// Extracts a single entry from a jar archive into Zed's data dir,
/// returning the cached path. Re-extracts when the cache file is older
/// than the archive on disk so that dependency upgrades don't show
/// stale source.
pub async fn extract_to_cache(jar: &Path, entry: &str) -> Result<PathBuf> {
    let mut hasher = Sha256::new();
    hasher.update(jar.as_os_str().as_encoded_bytes());
    let hash = format!("{:x}", hasher.finalize());

    // Use the first 16 hex chars — collisions are astronomically
    // unlikely in this scope and short paths keep file URIs sane.
    let cache_root = paths::data_dir().join("jar-extracts").join(&hash[..16]);
    let mut output = cache_root.clone();
    for seg in entry.split('/') {
        // Reject path components that would escape the cache root.
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(anyhow!("invalid jar entry segment: {seg:?}"));
        }
        output.push(seg);
    }

    if cache_is_fresh(jar, &output).await {
        return Ok(output);
    }

    let file = smol::fs::File::open(jar)
        .await
        .with_context(|| format!("opening jar {}", jar.display()))?;
    let buffered = smol::io::BufReader::new(file);
    let mut zip = ZipFileReader::new(buffered)
        .await
        .map_err(|e| anyhow!("reading zip {}: {e}", jar.display()))?;

    let target_idx = zip
        .file()
        .entries()
        .iter()
        .position(|e| {
            e.filename()
                .as_str()
                .map(|name| name == entry)
                .unwrap_or(false)
        })
        .with_context(|| format!("entry {entry:?} not found in {}", jar.display()))?;

    let mut reader = zip
        .reader_with_entry(target_idx)
        .await
        .map_err(|e| anyhow!("opening entry {entry:?}: {e}"))?;

    let mut bytes = Vec::new();
    reader
        .read_to_end_checked(&mut bytes)
        .await
        .map_err(|e| anyhow!("reading entry {entry:?} bytes: {e}"))?;

    if let Some(parent) = output.parent() {
        smol::fs::create_dir_all(parent).await?;
    }
    smol::fs::write(&output, &bytes).await?;
    set_read_only(&output).await;

    Ok(output)
}

/// `true` iff the cache file already exists and is at least as new as
/// the source jar — both modification times have to be readable for
/// us to consider the cache valid.
async fn cache_is_fresh(jar: &Path, cached: &Path) -> bool {
    let Ok(jar_meta) = smol::fs::metadata(jar).await else {
        return false;
    };
    let Ok(cached_meta) = smol::fs::metadata(cached).await else {
        return false;
    };
    let (Ok(jar_mtime), Ok(cached_mtime)) = (jar_meta.modified(), cached_meta.modified()) else {
        return false;
    };
    cached_mtime >= jar_mtime
}

#[cfg(unix)]
async fn set_read_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = smol::fs::metadata(path).await {
        let mut perms = metadata.permissions();
        // 0o444 — read-only for everyone. Edits to the cached file
        // would only confuse the user; LSP truth is in the jar.
        perms.set_mode(0o444);
        let _ = smol::fs::set_permissions(path, perms).await;
    }
}

#[cfg(not(unix))]
async fn set_read_only(path: &Path) {
    if let Ok(metadata) = smol::fs::metadata(path).await {
        let mut perms = metadata.permissions();
        perms.set_readonly(true);
        let _ = smol::fs::set_permissions(path, perms).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_kotlin_lsp_path() {
        let p = Path::new(
            "/Users/x/.gradle/caches/modules-2/files-2.1/org.jetbrains.kotlinx/kotlinx-coroutines-core-jvm/1.10.2/abc/kotlinx-coroutines-core-jvm-1.10.2-sources.jar!/commonMain/kotlinx/coroutines/CoroutineScope.kt",
        );
        let (jar, entry) = parse_jar_path(p).unwrap();
        assert!(jar.to_str().unwrap().ends_with(".jar"));
        assert_eq!(entry, "commonMain/kotlinx/coroutines/CoroutineScope.kt");
    }

    #[test]
    fn returns_none_for_plain_path() {
        let p = Path::new("/Users/x/repo/foo/Bar.kt");
        assert!(parse_jar_path(p).is_none());
    }

    #[test]
    fn returns_none_for_jar_without_entry() {
        let p = Path::new("/Users/x/foo.jar");
        assert!(parse_jar_path(p).is_none());
    }

    #[test]
    fn handles_exclamation_in_directory_components() {
        // The archive path itself may contain `!`; only the LAST
        // `.jar!/` should split.
        let p = Path::new("/tmp/weird!dir/foo.jar!/Foo.kt");
        let (jar, entry) = parse_jar_path(p).unwrap();
        assert_eq!(jar, Path::new("/tmp/weird!dir/foo.jar"));
        assert_eq!(entry, "Foo.kt");
    }
}
