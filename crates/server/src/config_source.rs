//! The `ConfigSource` abstraction (ADR 012): where the config document lives
//! and how it is read and atomically replaced, behind a trait so a future
//! source (e.g. SQLite, see the ADR's later phases) can sit next to the file
//! source without every caller learning a new API.
//!
//! [`FileSource`] is the extraction of the file read / staged-write machinery
//! that used to live inline in `admin::apply_config_document`: unique `.tmp`
//! staging, a `.bak` of the previous document, and an atomic rename, all
//! behind a compare-and-swap [`ConfigSource::persist`]. Validation is
//! deliberately NOT part of this module: the admin pipeline validates a
//! candidate document before ever calling `persist`, and the hot-reload path
//! re-validates independently after. `persist` only owns the "does this write
//! land" question, never "is this document any good".

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// A config document read from a [`ConfigSource`], paired with the content
/// hash a later `persist` must present as `expected_hash` to replace it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionedConfig {
    /// The document's contents, byte for byte (as UTF-8 text).
    pub toml: String,
    /// [`config_hash`] of `toml`'s bytes.
    pub hash: String,
}

/// Content hash of a config document: BLAKE3, lowercase hex.
///
/// Used as the concurrency token for `PUT /admin/config` and for every
/// [`ConfigSource::persist`] compare-and-swap. It is a change detector, not a
/// security boundary.
#[must_use]
pub fn config_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// The document a [`ConfigSource`] is deemed to hold before anything has ever
/// been written to it: the empty string. A DB-backed source with no row yet
/// starts here, the same way a file source starting from a zero-byte file
/// would.
pub const EMPTY_DOC: &str = "";

/// [`config_hash`] of [`EMPTY_DOC`], i.e. the hash a fresh, never-written
/// [`ConfigSource`] reports.
#[must_use]
pub fn empty_doc_hash() -> String {
    config_hash(EMPTY_DOC.as_bytes())
}

/// Failure from a [`ConfigSource`] operation.
///
/// Never reaches an HTTP client directly: callers (`admin::apply_config_document`
/// today) map this onto the existing `GatewayError` taxonomy, which already
/// carries the stable `LM-xxxx` codes documented in `docs/errors.md`.
#[derive(Debug, thiserror::Error)]
pub enum ConfigSourceError {
    /// A filesystem operation failed (read, write, rename, ...).
    #[error("config source I/O error: {0}")]
    Io(String),
    /// A `persist` compare-and-swap lost the race: the stored document no
    /// longer matches `expected_hash`. Carries the hash actually stored now,
    /// so the caller can decide whether to re-read and retry.
    #[error("config changed since it was read (current hash {current_hash})")]
    Stale {
        /// The hash of the document currently stored, as of this rejection.
        current_hash: String,
    },
    /// A database-backed source failed (reserved for a future `DbSource`;
    /// [`FileSource`] never produces this variant).
    #[error("config source database error: {0}")]
    Db(String),
    /// The stored bytes are not valid UTF-8, so they cannot be returned as a
    /// [`VersionedConfig::toml`] string.
    #[error("config document is not valid UTF-8")]
    NotUtf8,
}

/// Where the live config document lives, and how it is read and atomically
/// replaced.
///
/// Every method is safe to call from an async context: an implementation
/// that touches the filesystem or a database runs that work off the tokio
/// runtime (e.g. via `spawn_blocking`), never blocking a worker thread.
#[async_trait::async_trait]
pub trait ConfigSource: Send + Sync {
    /// Read the current document and its hash.
    async fn load(&self) -> Result<VersionedConfig, ConfigSourceError>;

    /// Compare-and-swap: replace the stored document with `toml` iff the
    /// document currently stored still hashes to `expected_hash`. Returns the
    /// new hash on success, or [`ConfigSourceError::Stale`] (carrying the
    /// hash actually stored) when another writer landed first.
    ///
    /// Does not validate `toml`: the caller is expected to have validated the
    /// candidate document already (see the module docs).
    async fn persist(&self, toml: &str, expected_hash: &str) -> Result<String, ConfigSourceError>;

    /// File path an external writer (a human editor, a GitOps sync) might
    /// change, for a hot-reload watcher to arm on. `None` means there is no
    /// such path - a change can only ever arrive through this trait's own
    /// `persist` (e.g. a future DB-backed source with no on-disk mirror).
    fn watch_path(&self) -> Option<&Path>;
}

/// [`ConfigSource`] backed by a plain file on disk: the only source LUMEN has
/// ever had, now behind the trait. The file stays the source of truth; `.bak`
/// of the previous document is kept alongside it on every successful
/// `persist`.
///
/// `Clone` (cheap: one `PathBuf`) is what lets `load`/`persist` below move an
/// owned copy of `self` into `tokio::task::spawn_blocking`'s `'static`
/// closure instead of trying to smuggle a borrow of `&self` across the
/// thread hop.
#[derive(Debug, Clone)]
pub struct FileSource {
    path: PathBuf,
}

impl FileSource {
    /// Back this source with the config file at `path`. Does not touch the
    /// filesystem; `path` need not exist yet (a first `load` will fail with
    /// [`ConfigSourceError::Io`] if it doesn't).
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Blocking half of [`ConfigSource::load`]: read `self.path` and hash it.
    /// Never call this from a runtime worker thread; only from
    /// `spawn_blocking` or another already-blocking context (e.g.
    /// `admin::apply_config_document`, which runs inside its own
    /// `spawn_blocking`).
    pub(crate) fn load_blocking(&self) -> Result<VersionedConfig, ConfigSourceError> {
        let bytes = std::fs::read(&self.path)
            .map_err(|e| ConfigSourceError::Io(format!("reading config: {e}")))?;
        let toml = String::from_utf8(bytes).map_err(|_| ConfigSourceError::NotUtf8)?;
        let hash = config_hash(toml.as_bytes());
        Ok(VersionedConfig { toml, hash })
    }

    /// Blocking half of [`ConfigSource::persist`]: the exact staged-write
    /// sequence that used to live inline in `admin::apply_config_document`
    /// (unique `.tmp` staging, permission preservation, a `.bak` of the
    /// previous document, atomic rename, best-effort directory fsync), with
    /// the CAS check re-run against the on-disk hash inside this same
    /// blocking section so a write racing a concurrent external edit (a human
    /// editor, a GitOps sync) is caught rather than silently lost. Never call
    /// this from a runtime worker thread; see [`Self::load_blocking`].
    pub(crate) fn persist_blocking(
        &self,
        toml: &str,
        expected_hash: &str,
    ) -> Result<String, ConfigSourceError> {
        let path = self.path.as_path();

        let current = std::fs::read(path)
            .map_err(|e| ConfigSourceError::Io(format!("reading config: {e}")))?;
        let current_hash = config_hash(&current);
        if current_hash != expected_hash {
            return Err(ConfigSourceError::Stale { current_hash });
        }

        // Unique per call (process id + a monotonic counter): a process that
        // was killed or crashed mid-persist can leave a stale `.tmp` behind,
        // and a fixed name would let a later call mistake it for its own
        // staging file. Same directory as `path` throughout - a rename is
        // only atomic within one filesystem.
        let staged = path.with_extension(format!("toml.{}.tmp", unique_suffix()));
        // Armed BEFORE the file is created, not after the write block: a
        // failing `write_all` or `sync_all` (a full disk is the realistic
        // trigger) returns through `?` with the file already created, and a
        // guard armed after the block would never see it. `commit()` is the
        // only way to suppress the removal, called exactly once, after the
        // rename that consumes the file.
        let staging = StagingGuard::new(&staged);
        {
            // `File::create` + `write_all` + `sync_all`, not `std::fs::write`:
            // `sync_all` is the step that actually matters here. `rename`
            // only makes the NAME change atomic; it says nothing about
            // whether the bytes behind the old name ever reached disk. On
            // ext4 with delayed allocation in particular, the rename's
            // metadata can be durable while the data blocks are not, so a
            // crash shortly after a persist can leave `path` pointing at a
            // zero-length file - `.bak` still holds the previous good
            // document, but nothing tells the operator to reach for it.
            // Flushing the data before the rename closes that window.
            let mut file = std::fs::File::create(&staged)
                .map_err(|e| ConfigSourceError::Io(format!("staging config: {e}")))?;
            file.write_all(toml.as_bytes())
                .map_err(|e| ConfigSourceError::Io(format!("staging config: {e}")))?;
            file.sync_all()
                .map_err(|e| ConfigSourceError::Io(format!("staging config: {e}")))?;
        }

        // `File::create` above always creates with mode `0o666 & !umask` -
        // typically `0644` - regardless of what `path` was actually set to,
        // and the live file adopts the STAGED file's mode on rename, not the
        // original's. Without this, a config file an operator hardened to
        // e.g. `0600` would silently widen to whatever the process umask
        // allows on the very first persist through this source, exposing
        // base URLs, model topology, `db_path` and `api_key_env` names to any
        // other local account on a shared host. Best-effort, like
        // `sync_parent_dir` below: on a filesystem with no Unix permission
        // model to speak of (CIFS/FAT-style mounts, some FUSE layers),
        // `chmod` fails not because a real permission would be widened, but
        // because there was never a permission bit to preserve in the first
        // place.
        if let Err(error) = preserve_permissions(path, &staged) {
            tracing::warn!(
                %error,
                path = %path.display(),
                "failed to preserve the config file's permissions across the apply; \
                 continuing, since a filesystem without a permission model has \
                 nothing to protect"
            );
        }

        let backup = path.with_extension("toml.bak");
        std::fs::copy(path, &backup)
            .map_err(|e| ConfigSourceError::Io(format!("backing up config: {e}")))?;
        std::fs::rename(&staged, path)
            .map_err(|e| ConfigSourceError::Io(format!("applying config: {e}")))?;
        staging.commit();

        // Fsync the containing directory too: a data-only fsync guarantees
        // the staged bytes are durable, but says nothing about whether the
        // RENAME itself (the directory-entry update that gives `path` its
        // new contents) survived a crash. Best-effort and non-fatal on
        // purpose: directory fsync is a documented no-op or an outright error
        // on some platforms (Windows in particular), and the persist has
        // already succeeded on disk by this point.
        sync_parent_dir(path);

        Ok(config_hash(toml.as_bytes()))
    }
}

#[async_trait::async_trait]
impl ConfigSource for FileSource {
    async fn load(&self) -> Result<VersionedConfig, ConfigSourceError> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.load_blocking())
            .await
            .map_err(|e| ConfigSourceError::Io(format!("load task failed: {e}")))?
    }

    async fn persist(&self, toml: &str, expected_hash: &str) -> Result<String, ConfigSourceError> {
        let this = self.clone();
        let toml = toml.to_owned();
        let expected_hash = expected_hash.to_owned();
        tokio::task::spawn_blocking(move || this.persist_blocking(&toml, &expected_hash))
            .await
            .map_err(|e| ConfigSourceError::Io(format!("persist task failed: {e}")))?
    }

    fn watch_path(&self) -> Option<&Path> {
        Some(&self.path)
    }
}

/// A process id + monotonic counter suffix, unique within this process's
/// lifetime. Not a security token - only meant to keep a crash-orphaned
/// staging file from colliding with a live one; the ordinary case is
/// serialised by the caller (`AppState::config_apply_lock` for the admin
/// route).
pub(crate) fn unique_suffix() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{n}", std::process::id())
}

/// Removes a staged file on drop, unless [`commit`](Self::commit) was called
/// first. Exists so every failure path out of a staged write - or, in
/// `admin::apply_config_document`, a rejected validation - cleans up its
/// staging file without each call site having to remember to.
pub(crate) struct StagingGuard<'a> {
    path: &'a Path,
    committed: bool,
}

impl<'a> StagingGuard<'a> {
    /// Guard `path`, the staged file, which need NOT exist yet: the guard is
    /// armed before creation so a failed write or fsync cannot leak a
    /// partially written file, and `Drop` ignores a path that is not there.
    pub(crate) fn new(path: &'a Path) -> Self {
        Self {
            path,
            committed: false,
        }
    }

    /// Declare the staged file consumed (renamed into place): `Drop` must
    /// not attempt to remove a path that no longer names the staging file.
    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for StagingGuard<'_> {
    fn drop(&mut self) {
        if !self.committed {
            // Best-effort: the file may already be gone (e.g. a concurrent
            // cleanup), and there is no client left to report a failure to.
            let _ = std::fs::remove_file(self.path);
        }
    }
}

/// Preserve `source`'s Unix file mode on `target`, best-effort.
///
/// `std::fs::File::create` always creates a new file with mode
/// `0o666 & !umask`, never the mode of any existing file at a neighbouring
/// path, so staging a config document in a fresh file and renaming it into
/// place would otherwise silently change the live file's permissions on
/// every persist. A no-op on non-Unix targets: there is no equivalent
/// permission-bit model to copy there.
///
/// The caller treats a returned error as best-effort too (log and continue):
/// on a filesystem that implements no Unix permission model at all
/// (CIFS/FAT-style mounts, some FUSE layers), `chmod` fails, but there was
/// never a permission to widen, so there is nothing to protect by refusing
/// the persist.
#[cfg(unix)]
fn preserve_permissions(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(source)?.permissions().mode();
    std::fs::set_permissions(target, std::fs::Permissions::from_mode(mode))
}

/// Non-Unix targets have no permission bits to copy.
#[cfg(not(unix))]
fn preserve_permissions(_source: &Path, _target: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Best-effort fsync of `path`'s containing directory, so a rename into
/// `path` is durable across a crash, not just present in the page cache.
/// Never returns an error: see the call site in [`FileSource::persist_blocking`]
/// for why a failure here must not fail a persist that already landed.
fn sync_parent_dir(path: &Path) {
    let parent = match path.parent() {
        // A bare filename (e.g. "lumen.toml", no directory component) has
        // an empty parent; its containing directory is the CWD.
        Some(p) if p.as_os_str().is_empty() => Path::new("."),
        Some(p) => p,
        None => return,
    };
    if let Err(error) = std::fs::File::open(parent).and_then(|dir| dir.sync_all()) {
        tracing::warn!(
            %error,
            path = %parent.display(),
            "failed to fsync the config directory after persisting a new config; \
             the rename may not survive a crash even though the persist itself succeeded"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_source_load_returns_bytes_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# hello\n[server]\nport = 8080\n").unwrap();
        let src = FileSource::new(path);
        let doc = src.load().await.unwrap();
        assert_eq!(doc.toml, "# hello\n[server]\nport = 8080\n");
        assert_eq!(doc.hash, config_hash(doc.toml.as_bytes()));
    }

    #[tokio::test]
    async fn file_source_persist_is_a_cas() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "a = 1\n").unwrap();
        let src = FileSource::new(path.clone());
        let doc = src.load().await.unwrap();
        // Simulate a human edit landing between load and persist.
        std::fs::write(&path, "a = 2\n").unwrap();
        let err = src.persist("a = 3\n", &doc.hash).await.unwrap_err();
        assert!(matches!(err, ConfigSourceError::Stale { .. }));
        // With the fresh hash it applies, and a .bak of the previous doc exists.
        let doc = src.load().await.unwrap();
        let new_hash = src.persist("a = 3\n", &doc.hash).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a = 3\n");
        assert_eq!(new_hash, config_hash(b"a = 3\n"));
        assert_eq!(
            std::fs::read_to_string(path.with_extension("toml.bak")).unwrap(),
            "a = 2\n"
        );
    }

    #[tokio::test]
    async fn empty_doc_hash_matches_config_hash_of_empty_bytes() {
        assert_eq!(empty_doc_hash(), config_hash(EMPTY_DOC.as_bytes()));
    }

    #[tokio::test]
    async fn file_source_watch_path_is_its_own_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let src = FileSource::new(path.clone());
        assert_eq!(src.watch_path(), Some(path.as_path()));
    }
}
