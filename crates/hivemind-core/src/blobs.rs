//! Content-addressed attachment storage (SPEC §4.3).
//!
//! Blobs live at `blobs/<sha256-hex>` and nowhere else. The name a sender
//! chose is metadata on the message, never a path — two people sending
//! `notes.md` share one file if the contents match, and neither can name a
//! file outside this directory.
//!
//! Incomplete downloads are kept as `<sha>.part` so a transfer interrupted by
//! a closed laptop resumes with a range request rather than starting again
//! (SPEC §13.2). A `.part` file is never promoted without its contents
//! hashing to the name it claims.

use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use crate::crypto::Sha256Digest;

/// Why a blob operation failed.
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    /// No blob with that digest here.
    #[error("no blob {digest}")]
    NotFound {
        /// The digest that was looked for.
        digest: String,
    },
    /// The filesystem said no.
    #[error("{context}")]
    Io {
        /// What we were trying to do.
        context: String,
        /// What went wrong.
        #[source]
        source: std::io::Error,
    },
    /// The bytes received did not hash to the digest they were filed under.
    ///
    /// Either the sender is confused or something changed them in flight. In
    /// both cases the only safe move is to throw them away.
    #[error("the content of {expected} hashes to {actual}")]
    DigestMismatch {
        /// What we asked for.
        expected: String,
        /// What arrived.
        actual: String,
    },
    /// The attachment is larger than this node accepts (SPEC §6.3).
    #[error("{name} is {size} bytes, over the limit of {limit}")]
    TooLarge {
        /// The attachment's name.
        name: String,
        /// How big it is.
        size: u64,
        /// The most this node takes.
        limit: u64,
    },
    /// The name is not a name.
    #[error("{0}")]
    UnsafeName(&'static str),
}

/// Check an attachment name is a file name and not a path (SPEC §6.3).
///
/// A recipient saves attachments by this name, so anything that could escape
/// the directory it is saved into — or overwrite something on the way — is
/// refused at the point it arrives rather than at the point it is used.
///
/// # Errors
/// [`BlobError::UnsafeName`] describing which rule it broke.
pub fn check_attachment_name(name: &str) -> Result<(), BlobError> {
    if name.is_empty() {
        return Err(BlobError::UnsafeName("an attachment needs a name"));
    }
    if name.len() > 255 {
        return Err(BlobError::UnsafeName(
            "an attachment name must be at most 255 bytes",
        ));
    }
    if name == "." || name == ".." {
        return Err(BlobError::UnsafeName(
            "an attachment cannot be named `.` or `..`",
        ));
    }
    // Both separators, on every platform: a name is checked where it arrives,
    // which may not be the kind of machine it was written on.
    if name.contains('/') || name.contains('\\') {
        return Err(BlobError::UnsafeName(
            "an attachment name cannot contain a path separator",
        ));
    }
    // A drive letter or a UNC prefix survives having its separators stripped.
    if name.contains(':') {
        return Err(BlobError::UnsafeName(
            "an attachment name cannot contain a colon",
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(BlobError::UnsafeName(
            "an attachment name cannot contain control characters",
        ));
    }
    // Leading and trailing spaces and dots are silently trimmed by Windows,
    // so `evil.exe ` and `evil.exe` are the same file there but not here.
    if name.trim() != name || name.ends_with('.') {
        return Err(BlobError::UnsafeName(
            "an attachment name cannot begin or end with whitespace, or end with a dot",
        ));
    }
    Ok(())
}

/// How much to read at a time when hashing or copying.
const CHUNK: usize = 64 * 1024;

/// The content-addressed blob directory.
#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Open (creating if needed) the blob directory at `root`.
    ///
    /// # Errors
    /// [`BlobError::Io`] if the directory cannot be created.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, BlobError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|source| BlobError::Io {
            context: format!("could not create {}", root.display()),
            source,
        })?;
        Ok(Self { root })
    }

    /// Where a blob lives once it is complete.
    #[must_use]
    pub fn path_of(&self, digest: &Sha256Digest) -> PathBuf {
        self.root.join(digest.to_hex())
    }

    /// Where a partial download accumulates.
    fn part_path(&self, digest: &Sha256Digest) -> PathBuf {
        self.root.join(format!("{}.part", digest.to_hex()))
    }

    /// Do we already hold this blob?
    ///
    /// The whole of deduplication: two people sending the same file is one
    /// file on disk, and a resend costs nothing.
    #[must_use]
    pub fn has(&self, digest: &Sha256Digest) -> bool {
        self.path_of(digest).is_file()
    }

    /// How big a blob we hold is.
    #[must_use]
    pub fn size_of(&self, digest: &Sha256Digest) -> Option<u64> {
        fs::metadata(self.path_of(digest)).ok().map(|m| m.len())
    }

    /// Open a complete blob for reading.
    ///
    /// # Errors
    /// [`BlobError::NotFound`] if we do not hold it.
    pub fn open_read(&self, digest: &Sha256Digest) -> Result<fs::File, BlobError> {
        fs::File::open(self.path_of(digest)).map_err(|_| BlobError::NotFound {
            digest: digest.to_hex(),
        })
    }

    /// Take a copy of a local file, returning what it hashed to and its size.
    ///
    /// The file is hashed as it is copied rather than read twice: an
    /// attachment can be gigabytes, and reading it twice doubles the slowest
    /// part of sending one.
    ///
    /// # Errors
    /// [`BlobError::TooLarge`] if it is over `limit`, or [`BlobError::Io`].
    pub fn put_file(&self, path: &Path, limit: u64) -> Result<(Sha256Digest, u64), BlobError> {
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );

        let mut source = fs::File::open(path).map_err(|source| BlobError::Io {
            context: format!("could not read {}", path.display()),
            source,
        })?;

        // Checked before reading, so an oversized file is refused rather than
        // streamed to a temporary the caller then has to clean up.
        let size = source
            .metadata()
            .map_err(|source| BlobError::Io {
                context: format!("could not stat {}", path.display()),
                source,
            })?
            .len();
        if size > limit {
            return Err(BlobError::TooLarge { name, size, limit });
        }

        let temporary = self
            .root
            .join(format!(".incoming-{}", ulid::Ulid::generate()));
        let mut sink = fs::File::create(&temporary).map_err(|source| BlobError::Io {
            context: format!("could not create {}", temporary.display()),
            source,
        })?;

        let mut hasher = Sha256Digest::hasher();
        let mut buffer = vec![0u8; CHUNK];
        let mut written = 0u64;
        loop {
            let read = match source.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(source) => {
                    let _ = fs::remove_file(&temporary);
                    return Err(BlobError::Io {
                        context: format!("could not read {}", path.display()),
                        source,
                    });
                }
            };
            // The size can change between the stat above and here — a log file
            // being appended to, say — so the limit is enforced again on the
            // bytes actually seen.
            written += read as u64;
            if written > limit {
                let _ = fs::remove_file(&temporary);
                return Err(BlobError::TooLarge {
                    name,
                    size: written,
                    limit,
                });
            }
            hasher.update(&buffer[..read]);
            if let Err(source) = sink.write_all(&buffer[..read]) {
                let _ = fs::remove_file(&temporary);
                return Err(BlobError::Io {
                    context: format!("could not write {}", temporary.display()),
                    source,
                });
            }
        }

        let digest = hasher.finish();
        self.promote(&temporary, &digest)?;
        Ok((digest, written))
    }

    /// Store bytes already in memory, returning what they hashed to.
    ///
    /// # Errors
    /// [`BlobError::Io`] if the write fails.
    pub fn put_bytes(&self, bytes: &[u8]) -> Result<Sha256Digest, BlobError> {
        let digest = Sha256Digest::of(bytes);
        if self.has(&digest) {
            return Ok(digest);
        }

        let temporary = self
            .root
            .join(format!(".incoming-{}", ulid::Ulid::generate()));
        fs::write(&temporary, bytes).map_err(|source| BlobError::Io {
            context: format!("could not write {}", temporary.display()),
            source,
        })?;
        self.promote(&temporary, &digest)?;
        Ok(digest)
    }

    /// Move a finished temporary into place.
    ///
    /// A rename onto an existing blob is a no-op rather than a conflict: the
    /// name *is* the contents, so whoever got there first wrote the same bytes.
    fn promote(&self, temporary: &Path, digest: &Sha256Digest) -> Result<(), BlobError> {
        let final_path = self.path_of(digest);
        if final_path.exists() {
            let _ = fs::remove_file(temporary);
            return Ok(());
        }
        fs::rename(temporary, &final_path).map_err(|source| {
            let _ = fs::remove_file(temporary);
            BlobError::Io {
                context: format!("could not move a blob into {}", final_path.display()),
                source,
            }
        })
    }

    /// How many bytes of an interrupted download we already hold.
    ///
    /// This is the offset a range request resumes from (SPEC §7.2).
    #[must_use]
    pub fn partial_len(&self, digest: &Sha256Digest) -> u64 {
        fs::metadata(self.part_path(digest)).map_or(0, |m| m.len())
    }

    /// Append `chunk` to an in-progress download.
    ///
    /// # Errors
    /// [`BlobError::Io`] if the append fails.
    pub fn append_partial(&self, digest: &Sha256Digest, chunk: &[u8]) -> Result<u64, BlobError> {
        let path = self.part_path(digest);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| BlobError::Io {
                context: format!("could not open {}", path.display()),
                source,
            })?;
        file.write_all(chunk).map_err(|source| BlobError::Io {
            context: format!("could not append to {}", path.display()),
            source,
        })?;
        Ok(self.partial_len(digest))
    }

    /// Finish a download: check the bytes are what they claim, then keep them.
    ///
    /// A mismatch throws the partial away. Either the sender is confused or
    /// something changed the bytes in flight, and resuming from a corrupt
    /// prefix would never converge.
    ///
    /// # Errors
    /// [`BlobError::DigestMismatch`] if the contents are not what was asked
    /// for, or [`BlobError::Io`].
    pub fn finish_partial(&self, digest: &Sha256Digest) -> Result<(), BlobError> {
        let path = self.part_path(digest);
        let mut file = fs::File::open(&path).map_err(|source| BlobError::Io {
            context: format!("could not read {}", path.display()),
            source,
        })?;

        let mut hasher = Sha256Digest::hasher();
        let mut buffer = vec![0u8; CHUNK];
        loop {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => hasher.update(&buffer[..read]),
                Err(source) => {
                    return Err(BlobError::Io {
                        context: format!("could not read {}", path.display()),
                        source,
                    });
                }
            }
        }

        let actual = hasher.finish();
        if actual != *digest {
            let _ = fs::remove_file(&path);
            return Err(BlobError::DigestMismatch {
                expected: digest.to_hex(),
                actual: actual.to_hex(),
            });
        }

        drop(file);
        self.promote(&path, digest)
    }

    /// Throw away an in-progress download.
    pub fn discard_partial(&self, digest: &Sha256Digest) {
        let _ = fs::remove_file(self.part_path(digest));
    }
}

#[cfg(test)]
mod name_tests {
    use super::*;

    #[track_caller]
    fn refused(name: &str) {
        assert!(
            check_attachment_name(name).is_err(),
            "{name:?} should have been refused"
        );
    }

    #[track_caller]
    fn allowed(name: &str) {
        check_attachment_name(name)
            .unwrap_or_else(|e| panic!("{name:?} should have been allowed: {e}"));
    }

    #[test]
    fn ordinary_file_names_are_allowed() {
        allowed("notes.md");
        allowed("Screenshot 2026-09-18 at 10.42.png");
        allowed("relatório-final.pdf");
        allowed("...leading dots are fine");
        allowed("a");
    }

    #[test]
    fn nothing_that_could_escape_the_directory_is_allowed() {
        refused("../../etc/passwd");
        refused("..");
        refused(".");
        refused("/etc/passwd");
        refused("sub/dir.txt");
        refused("windows\\style.txt");
        refused("C:\\Windows\\System32\\evil.dll");
        // A colon alone is enough on Windows, where `C:notes.md` is relative
        // to the current directory *on drive C*.
        refused("C:notes.md");
    }

    #[test]
    fn a_name_that_two_systems_would_read_differently_is_refused() {
        // Windows trims trailing spaces and dots, so these collide with names
        // that do not look like them.
        refused("evil.exe ");
        refused(" evil.exe");
        refused("evil.exe.");
    }

    #[test]
    fn a_name_that_could_rewrite_a_terminal_is_refused() {
        // `hivemind inbox` prints these.
        refused("innocent\u{1b}[2Kmalicious.sh");
        refused("with\na newline");
        refused("with\0a nul");
    }

    #[test]
    fn a_name_must_exist_and_must_fit() {
        refused("");
        refused(&"a".repeat(256));
        allowed(&"a".repeat(255));
    }
}

#[cfg(test)]
mod store_tests {
    use super::*;

    const NO_LIMIT: u64 = u64::MAX;

    fn store() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = BlobStore::open(dir.path().join("blobs")).expect("store opens");
        (dir, store)
    }

    fn read_back(store: &BlobStore, digest: &Sha256Digest) -> Vec<u8> {
        let mut file = store.open_read(digest).expect("open");
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes).expect("read");
        bytes
    }

    #[test]
    fn a_blob_is_named_by_what_is_in_it() {
        let (_dir, store) = store();
        let digest = store.put_bytes(b"hello").expect("put");

        assert_eq!(digest, Sha256Digest::of(b"hello"));
        assert_eq!(
            store.path_of(&digest).file_name().expect("a name"),
            digest.to_hex().as_str(),
            "the file name is the digest, so there is nowhere else it could go"
        );
        assert_eq!(read_back(&store, &digest), b"hello");
    }

    #[test]
    fn the_same_content_twice_is_one_file() {
        // SPEC §4.3: deduplicated. Two people sending the same screenshot
        // should cost one copy.
        let (dir, store) = store();
        let first = store.put_bytes(b"the same bytes").expect("put");
        let second = store.put_bytes(b"the same bytes").expect("put again");

        assert_eq!(first, second);
        let files: Vec<_> = fs::read_dir(dir.path().join("blobs"))
            .expect("read dir")
            .filter_map(Result::ok)
            .collect();
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn a_file_is_hashed_as_it_is_copied() {
        let (dir, store) = store();
        let source = dir.path().join("notes.md");
        // Larger than one chunk, so the incremental path is what runs.
        let content = "x".repeat(CHUNK * 3 + 17);
        fs::write(&source, &content).expect("write");

        let (digest, size) = store.put_file(&source, NO_LIMIT).expect("put_file");

        assert_eq!(digest, Sha256Digest::of(content.as_bytes()));
        assert_eq!(size, content.len() as u64);
        assert_eq!(read_back(&store, &digest), content.as_bytes());
    }

    #[test]
    fn a_file_over_the_limit_is_refused_and_leaves_nothing_behind() {
        let (dir, store) = store();
        let source = dir.path().join("huge.bin");
        fs::write(&source, vec![0u8; 4096]).expect("write");

        let error = store.put_file(&source, 1024).expect_err("over the limit");
        assert!(matches!(error, BlobError::TooLarge { size: 4096, .. }));

        let leftovers: Vec<_> = fs::read_dir(dir.path().join("blobs"))
            .expect("read dir")
            .filter_map(Result::ok)
            .collect();
        assert!(
            leftovers.is_empty(),
            "a refusal should not leave a temporary"
        );
    }

    #[test]
    fn asking_for_a_blob_we_do_not_have_says_so() {
        let (_dir, store) = store();
        let missing = Sha256Digest::of(b"never stored");

        assert!(!store.has(&missing));
        assert_eq!(store.size_of(&missing), None);
        assert!(matches!(
            store.open_read(&missing),
            Err(BlobError::NotFound { .. })
        ));
    }

    #[test]
    fn an_interrupted_download_resumes_from_where_it_stopped() {
        // SPEC §13.2: kill the transfer mid-way and assert it resumes with a
        // range request. This is the half of that which lives on disk.
        let (_dir, store) = store();
        let content = b"the whole file, eventually";
        let digest = Sha256Digest::of(content);

        assert_eq!(store.partial_len(&digest), 0, "nothing yet");
        let after = store
            .append_partial(&digest, &content[..10])
            .expect("append");
        assert_eq!(after, 10, "that is where a range request would resume");
        assert!(!store.has(&digest), "an unfinished blob is not a blob");

        store
            .append_partial(&digest, &content[10..])
            .expect("append the rest");
        store.finish_partial(&digest).expect("finish");

        assert!(store.has(&digest));
        assert_eq!(read_back(&store, &digest), content);
        assert_eq!(store.partial_len(&digest), 0, "the partial is gone");
    }

    #[test]
    fn bytes_that_are_not_what_they_claim_are_thrown_away() {
        // Resuming from a corrupt prefix would never converge, and keeping it
        // would mean serving something other than what the digest promises.
        let (_dir, store) = store();
        let digest = Sha256Digest::of(b"what was promised");

        store
            .append_partial(&digest, b"something else entirely")
            .expect("append");
        let error = store
            .finish_partial(&digest)
            .expect_err("should not verify");

        assert!(matches!(error, BlobError::DigestMismatch { .. }));
        assert!(!store.has(&digest));
        assert_eq!(
            store.partial_len(&digest),
            0,
            "the bad partial must not be left to be resumed"
        );
    }

    #[test]
    fn a_download_of_something_we_already_have_does_not_disturb_it() {
        let (_dir, store) = store();
        let digest = store.put_bytes(b"already here").expect("put");

        // A second sender delivering the same attachment.
        store
            .append_partial(&digest, b"already here")
            .expect("append");
        store.finish_partial(&digest).expect("finish");

        assert_eq!(read_back(&store, &digest), b"already here");
    }

    #[test]
    fn a_partial_can_be_abandoned() {
        let (_dir, store) = store();
        let digest = Sha256Digest::of(b"gave up on this");

        store.append_partial(&digest, b"gave up").expect("append");
        assert_eq!(store.partial_len(&digest), 7);

        store.discard_partial(&digest);
        assert_eq!(store.partial_len(&digest), 0);
    }

    #[test]
    fn a_digest_survives_being_written_down_and_read_back() {
        // It travels in a URL path, so this parses what a stranger typed.
        let digest = Sha256Digest::of(b"round trip");
        assert_eq!(
            digest.to_hex().parse::<Sha256Digest>().expect("parses"),
            digest
        );

        assert!("".parse::<Sha256Digest>().is_err());
        assert!("../../etc/passwd".parse::<Sha256Digest>().is_err());
        assert!("zz".repeat(32).parse::<Sha256Digest>().is_err());
        assert!(
            digest
                .to_hex()
                .to_uppercase()
                .parse::<Sha256Digest>()
                .is_ok()
        );
    }
}
