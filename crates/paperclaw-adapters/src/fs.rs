//! Filesystem-backed adapters: inbox source and library store.
//!
//! The store writes three sibling files for each ingested document:
//!
//! ```text
//! library/<category>/<stem>.pdf
//! library/<category>/<stem>.md
//! library/<category>/<stem>.paperclaw.json
//! ```
//!
//! …plus a JSONL append to `library/_logs/ingest-<date>.jsonl` for each
//! ingest entry. Collisions are resolved by suffixing the stem with
//! `-2`, `-3`, … until a free slot opens.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use paperclaw_domain::ports::{InboxError, StoreError};
use paperclaw_domain::types::{
    Classification, DocumentId, IngestEntry, LibraryPath, PendingDocument, SourcePath, Transcript,
};
use paperclaw_domain::{InboxSource, LibraryStore};
use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::{debug, instrument, warn};

/// Filesystem-backed inbox. Reads PDFs from a directory; non-PDF entries
/// are ignored.
#[derive(Debug, Clone)]
pub struct FsInboxSource {
    root: PathBuf,
}

impl FsInboxSource {
    /// Construct an inbox rooted at `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { root: path.into() }
    }
}

#[async_trait]
impl InboxSource for FsInboxSource {
    #[instrument(skip(self), fields(root = %self.root.display()))]
    async fn pending(&self) -> Result<Vec<PendingDocument>, InboxError> {
        if !fs::try_exists(&self.root)
            .await
            .map_err(|e| InboxError::Io(e.to_string()))?
        {
            return Err(InboxError::Unavailable(format!(
                "inbox path does not exist: {}",
                self.root.display(),
            )));
        }

        let mut entries = fs::read_dir(&self.root)
            .await
            .map_err(|e| InboxError::Io(e.to_string()))?;

        let mut out = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| InboxError::Io(e.to_string()))?
        {
            let path = entry.path();

            // `symlink_metadata` does NOT follow symlinks; `Path::is_file`
            // does. A symlink in the inbox could point anywhere on disk,
            // so we refuse to read it and surface a warning instead.
            let meta = fs::symlink_metadata(&path)
                .await
                .map_err(|e| InboxError::Io(e.to_string()))?;
            if meta.is_symlink() {
                warn!(
                    path = %path.display(),
                    "refusing to ingest symlinked inbox entry; \
                     copy the file in place instead",
                );
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            if path.extension().and_then(std::ffi::OsStr::to_str) != Some("pdf") {
                debug!(path = %path.display(), "skipping non-PDF inbox entry");
                continue;
            }
            let bytes = fs::read(&path)
                .await
                .map_err(|e| InboxError::Io(e.to_string()))?;
            out.push(PendingDocument {
                source: SourcePath::new(path),
                bytes,
            });
        }
        out.sort_by(|a, b| a.source.as_path().cmp(b.source.as_path()));
        Ok(out)
    }

    #[instrument(skip(self), fields(source = %source.as_path().display()))]
    async fn consume(&self, source: &SourcePath) -> Result<(), InboxError> {
        let path = source.as_path();

        // Same defense in depth as `pending`: never follow a symlink at
        // delete time. The pending scan already filters them out, but a
        // long-running ingest could race with someone swapping a real
        // file for a symlink between scan and consume.
        match fs::symlink_metadata(path).await {
            Ok(meta) => {
                if meta.is_symlink() {
                    return Err(InboxError::Io(format!(
                        "refusing to delete symlinked inbox entry: {}",
                        path.display(),
                    )));
                }
                if !meta.is_file() {
                    return Err(InboxError::Io(format!(
                        "inbox entry is not a regular file: {}",
                        path.display(),
                    )));
                }
            }
            // Already gone is fine — the post-condition (entry absent
            // from inbox) is satisfied.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(InboxError::Io(e.to_string())),
        }

        fs::remove_file(path)
            .await
            .map_err(|e| InboxError::Io(e.to_string()))
    }
}

/// Filesystem-backed library store. Persists `(pdf, transcript, metadata)`
/// triples and appends structured ingest log entries.
#[derive(Debug, Clone)]
pub struct FsLibraryStore {
    root: PathBuf,
}

impl FsLibraryStore {
    /// Construct a store rooted at `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { root: path.into() }
    }

    fn category_dir(&self, target: &LibraryPath) -> PathBuf {
        self.root.join(&target.category)
    }

    fn logs_dir(&self) -> PathBuf {
        self.root.join("_logs")
    }
}

#[async_trait]
impl LibraryStore for FsLibraryStore {
    #[instrument(skip_all, fields(target = ?target))]
    async fn store(
        &self,
        target: &LibraryPath,
        pdf_bytes: &[u8],
        transcript: &Transcript,
        classification: &Classification,
        ingested_at: OffsetDateTime,
        id: DocumentId,
    ) -> Result<LibraryPath, StoreError> {
        let dir = self.category_dir(target);
        fs::create_dir_all(&dir)
            .await
            .map_err(|e| StoreError::Io(e.to_string()))?;

        let resolved = resolve_collision(&dir, &target.stem).await?;
        let stem_path = dir.join(&resolved);

        // Build all three payloads up-front so we can fail fast on
        // serialization errors before touching disk.
        let markdown = build_markdown(classification, transcript);
        let sidecar = MetadataSidecar {
            id,
            ingested_at: ingested_at
                .format(&Rfc3339)
                .map_err(|e| StoreError::Serialization(e.to_string()))?,
            classification,
            transcript_bytes: transcript.as_str().len(),
            pdf_bytes: pdf_bytes.len(),
            schema_version: 1,
        };
        let meta_bytes = serde_json::to_vec_pretty(&sidecar)
            .map_err(|e| StoreError::Serialization(e.to_string()))?;

        // Atomic writes: each sibling file is written to a `.tmp` name,
        // fsynced, then renamed over the final path. POSIX `rename` is
        // atomic per file, so a crash mid-batch can leave at most some
        // of the three siblings present — never a half-written one.
        let pdf_path = with_extension(&stem_path, "pdf");
        write_atomic(&pdf_path, pdf_bytes).await?;

        let md_path = with_extension(&stem_path, "md");
        write_atomic(&md_path, markdown.as_bytes()).await?;

        let meta_path = stem_path.with_extension("paperclaw.json");
        write_atomic(&meta_path, &meta_bytes).await?;

        Ok(LibraryPath {
            category: target.category.clone(),
            stem: resolved,
        })
    }

    #[instrument(skip_all)]
    async fn append_log(&self, entry: &IngestEntry) -> Result<(), StoreError> {
        let dir = self.logs_dir();
        fs::create_dir_all(&dir)
            .await
            .map_err(|e| StoreError::Io(e.to_string()))?;

        let today = OffsetDateTime::now_utc().date();
        let date_str = today
            .format(time::macros::format_description!("[year]-[month]-[day]"))
            .map_err(|e| StoreError::Serialization(e.to_string()))?;
        let log_path = dir.join(format!("ingest-{date_str}.jsonl"));

        let mut line =
            serde_json::to_vec(entry).map_err(|e| StoreError::Serialization(e.to_string()))?;
        line.push(b'\n');

        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .await
            .map_err(|e| StoreError::Io(e.to_string()))?;
        f.write_all(&line)
            .await
            .map_err(|e| StoreError::Io(e.to_string()))?;
        f.flush().await.map_err(|e| StoreError::Io(e.to_string()))?;
        Ok(())
    }
}

#[derive(Serialize)]
struct MetadataSidecar<'a> {
    id: DocumentId,
    ingested_at: String,
    classification: &'a Classification,
    transcript_bytes: usize,
    pdf_bytes: usize,
    schema_version: u32,
}

fn build_markdown(classification: &Classification, transcript: &Transcript) -> String {
    let mut out = String::with_capacity(transcript.as_str().len() + 256);
    out.push_str("---\n");
    // `write!` into a String is infallible; ignoring the Result is the
    // idiomatic pattern here.
    let _ = writeln!(out, "kind: {}", classification.kind.folder_slug());
    let _ = writeln!(out, "confidence: {:.2}", classification.confidence.value(),);
    if let Some(sender) = &classification.sender {
        let _ = writeln!(out, "sender: {sender}");
    }
    if let Some(subject) = &classification.subject {
        let _ = writeln!(out, "subject: {subject}");
    }
    if let Some(date) = classification.document_date {
        let formatted = date
            .format(time::macros::format_description!("[year]-[month]-[day]"))
            .unwrap_or_else(|_| "unknown-date".to_owned());
        let _ = writeln!(out, "document_date: {formatted}");
    }
    out.push_str("---\n\n");
    out.push_str(transcript.as_str());
    if !transcript.as_str().ends_with('\n') {
        out.push('\n');
    }
    out
}

fn with_extension(stem_path: &Path, ext: &str) -> PathBuf {
    let mut p = stem_path.to_path_buf();
    p.set_extension(ext);
    p
}

/// Write `bytes` to `final_path` atomically: create a sibling `.tmp`
/// file, write+fsync, then rename onto the final name.
///
/// We intentionally do not fsync the containing directory after the
/// rename. Crash-during-ingest can therefore lose a freshly-renamed file
/// on power loss without an fsck — that trade-off is documented in
/// `docs/DESIGN.md` and acceptable for a single-user personal library.
async fn write_atomic(final_path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let tmp_path = with_tmp_suffix(final_path);
    let mut f = fs::File::create(&tmp_path)
        .await
        .map_err(|e| StoreError::Io(e.to_string()))?;
    f.write_all(bytes)
        .await
        .map_err(|e| StoreError::Io(e.to_string()))?;
    f.sync_all()
        .await
        .map_err(|e| StoreError::Io(e.to_string()))?;
    drop(f);
    fs::rename(&tmp_path, final_path)
        .await
        .map_err(|e| StoreError::Io(e.to_string()))?;
    Ok(())
}

fn with_tmp_suffix(path: &Path) -> PathBuf {
    // Append `.tmp` to whatever filename we were given so the tmp file
    // sits next to the final file (same filesystem → rename is atomic).
    let mut name = path
        .file_name()
        .map(std::ffi::OsString::from)
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

async fn resolve_collision(dir: &Path, stem: &str) -> Result<String, StoreError> {
    // Probe in order: stem, stem-2, stem-3, … until none of the three
    // sibling files exist.
    let mut candidate = stem.to_owned();
    let mut suffix: u32 = 1;
    loop {
        let pdf = dir.join(format!("{candidate}.pdf"));
        let md = dir.join(format!("{candidate}.md"));
        let meta = dir.join(format!("{candidate}.paperclaw.json"));
        let pdf_exists = fs::try_exists(&pdf)
            .await
            .map_err(|e| StoreError::Io(e.to_string()))?;
        let md_exists = fs::try_exists(&md)
            .await
            .map_err(|e| StoreError::Io(e.to_string()))?;
        let meta_exists = fs::try_exists(&meta)
            .await
            .map_err(|e| StoreError::Io(e.to_string()))?;
        if !pdf_exists && !md_exists && !meta_exists {
            return Ok(candidate);
        }
        suffix += 1;
        candidate = format!("{stem}-{suffix}");
        if suffix > 9999 {
            return Err(StoreError::Io("collision suffix exhausted".to_owned()));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use paperclaw_domain::testing::assert_send;
    use paperclaw_domain::types::{Confidence, DocumentKind};
    use tempfile::TempDir;
    use tokio::io::AsyncReadExt;
    use uuid::Uuid;

    use super::*;

    const _: fn() = || {
        assert_send::<FsInboxSource>();
        assert_send::<FsLibraryStore>();
    };

    #[tokio::test]
    async fn inbox_lists_pdfs_and_ignores_other_files() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.pdf"), b"%PDF-1.4 a")
            .await
            .unwrap();
        fs::write(dir.path().join("b.pdf"), b"%PDF-1.4 b")
            .await
            .unwrap();
        fs::write(dir.path().join("notes.txt"), b"ignore me")
            .await
            .unwrap();

        let inbox = FsInboxSource::new(dir.path());
        let pending = inbox.pending().await.unwrap();
        assert_eq!(pending.len(), 2);
        let names: Vec<_> = pending
            .iter()
            .map(|p| {
                p.source
                    .as_path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["a.pdf".to_owned(), "b.pdf".to_owned()]);
    }

    #[tokio::test]
    async fn inbox_errors_when_path_missing() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist");
        let inbox = FsInboxSource::new(missing);
        let err = inbox.pending().await.unwrap_err();
        assert!(matches!(err, InboxError::Unavailable(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn inbox_skips_symlinked_pdfs() {
        let outside = TempDir::new().unwrap();
        let secret = outside.path().join("secret.pdf");
        fs::write(&secret, b"%PDF-1.4 secret").await.unwrap();

        let inbox_dir = TempDir::new().unwrap();
        // A real PDF + a symlink that aims at a file outside the inbox.
        fs::write(inbox_dir.path().join("a.pdf"), b"%PDF-1.4 a")
            .await
            .unwrap();
        std::os::unix::fs::symlink(&secret, inbox_dir.path().join("link.pdf")).unwrap();

        let inbox = FsInboxSource::new(inbox_dir.path());
        let pending = inbox.pending().await.unwrap();
        let names: Vec<_> = pending
            .iter()
            .map(|p| {
                p.source
                    .as_path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["a.pdf".to_owned()]);
    }

    #[tokio::test]
    async fn inbox_consume_removes_the_source_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.pdf");
        fs::write(&path, b"%PDF-1.4 a").await.unwrap();

        let inbox = FsInboxSource::new(dir.path());
        inbox.consume(&SourcePath::new(&path)).await.unwrap();
        assert!(!path.exists(), "consume should delete the source file");
    }

    #[tokio::test]
    async fn inbox_consume_is_idempotent_for_missing_files() {
        let dir = TempDir::new().unwrap();
        let inbox = FsInboxSource::new(dir.path());
        // Never created — consume should still succeed (post-condition met).
        inbox
            .consume(&SourcePath::new(dir.path().join("ghost.pdf")))
            .await
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn inbox_consume_refuses_to_follow_symlinks() {
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("real.pdf");
        fs::write(&target, b"%PDF-1.4").await.unwrap();

        let inbox_dir = TempDir::new().unwrap();
        let link = inbox_dir.path().join("link.pdf");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let inbox = FsInboxSource::new(inbox_dir.path());
        let err = inbox.consume(&SourcePath::new(&link)).await.unwrap_err();
        assert!(matches!(err, InboxError::Io(_)));
        // The thing the symlink points at must still be there.
        assert!(target.exists(), "symlink target must not be deleted");
    }

    #[tokio::test]
    async fn store_writes_three_sibling_files_and_appends_log() {
        let dir = TempDir::new().unwrap();
        let store = FsLibraryStore::new(dir.path());

        let target = LibraryPath {
            category: "invoice".into(),
            stem: "2026-05-13_acme_inv-1".into(),
        };
        let classification = Classification {
            kind: DocumentKind::Invoice,
            confidence: Confidence::new(0.95),
            sender: Some("Acme".into()),
            subject: Some("Invoice 1".into()),
            document_date: None,
            rationale: None,
        };
        let id = DocumentId::from_uuid(Uuid::nil());
        let transcript = Transcript::new("Hello world");

        let resolved = store
            .store(
                &target,
                b"%PDF-1.4 fake",
                &transcript,
                &classification,
                OffsetDateTime::now_utc(),
                id,
            )
            .await
            .unwrap();
        assert_eq!(resolved.stem, target.stem);

        let pdf = dir.path().join("invoice").join("2026-05-13_acme_inv-1.pdf");
        let md = dir.path().join("invoice").join("2026-05-13_acme_inv-1.md");
        let meta = dir
            .path()
            .join("invoice")
            .join("2026-05-13_acme_inv-1.paperclaw.json");
        assert!(pdf.exists());
        assert!(md.exists());
        assert!(meta.exists());

        // Atomic-write side effect: no stray .tmp files remain after a
        // successful store. Catches regressions where rename is skipped
        // or a tmp path leaks.
        let mut entries = fs::read_dir(dir.path().join("invoice")).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.ends_with(".tmp"),
                "leftover .tmp file after store: {name}",
            );
        }

        let md_contents = fs::read_to_string(md).await.unwrap();
        assert!(md_contents.contains("Hello world"));
        assert!(md_contents.starts_with("---\n"));

        // Append a log entry; expect a JSONL file.
        let entry = IngestEntry {
            source: SourcePath::new("inbox/a.pdf"),
            outcome: paperclaw_domain::types::IngestOutcome::SkippedEncrypted {
                hint: "test".into(),
            },
        };
        store.append_log(&entry).await.unwrap();
        let logs_dir = dir.path().join("_logs");
        let mut entries = fs::read_dir(&logs_dir).await.unwrap();
        let mut found = false;
        while let Some(e) = entries.next_entry().await.unwrap() {
            let mut buf = String::new();
            let mut f = fs::File::open(e.path()).await.unwrap();
            f.read_to_string(&mut buf).await.unwrap();
            if buf.contains("SkippedEncrypted") {
                found = true;
            }
        }
        assert!(found, "log file should contain the entry");
    }

    #[tokio::test]
    async fn store_resolves_collisions_with_numeric_suffix() {
        let dir = TempDir::new().unwrap();
        let store = FsLibraryStore::new(dir.path());

        let target = LibraryPath {
            category: "invoice".into(),
            stem: "dup".into(),
        };
        let classification = Classification {
            kind: DocumentKind::Invoice,
            confidence: Confidence::new(0.95),
            sender: None,
            subject: None,
            document_date: None,
            rationale: None,
        };

        let first = store
            .store(
                &target,
                b"a",
                &Transcript::new("a"),
                &classification,
                OffsetDateTime::now_utc(),
                DocumentId::from_uuid(Uuid::nil()),
            )
            .await
            .unwrap();
        let second = store
            .store(
                &target,
                b"b",
                &Transcript::new("b"),
                &classification,
                OffsetDateTime::now_utc(),
                DocumentId::from_uuid(Uuid::nil()),
            )
            .await
            .unwrap();

        assert_eq!(first.stem, "dup");
        assert_eq!(second.stem, "dup-2");

        // Force a third one to ensure suffix grows monotonically.
        let third = Arc::new(store)
            .store(
                &target,
                b"c",
                &Transcript::new("c"),
                &classification,
                OffsetDateTime::now_utc(),
                DocumentId::from_uuid(Uuid::nil()),
            )
            .await
            .unwrap();
        assert_eq!(third.stem, "dup-3");
    }
}
