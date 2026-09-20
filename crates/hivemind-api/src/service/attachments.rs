//! Files travelling with a message, or fetched after it (SPEC §8).
//!
//! Whether an attachment ships inline or as a reference is decided when the
//! message is signed, so the recipient knows which to expect. Everything else
//! here follows from that choice: storing what arrived inline, fetching what
//! did not, and the limits that bound one delivery.

use super::*;

impl MailService {
    /// Copy local files into the blob store and describe them (SPEC §8).
    ///
    /// Each file is hashed as it is copied, so an attachment sent twice — or
    /// sent by two people — is stored once. Whether it travels with the
    /// message or is fetched on demand is decided here and recorded in the
    /// signed message, so the recipient knows which to expect.
    pub(super) fn take_attachments(
        &self,
        paths: &[std::path::PathBuf],
    ) -> Result<Vec<AttachmentRef>, ServiceError> {
        let mut refs = Vec::with_capacity(paths.len());
        // Bounded so that one message cannot become an unboundedly large
        // delivery. Past the budget, otherwise-inline files ship as refs and
        // the recipient fetches them; nothing is refused for being numerous.
        let mut inline_budget = self.inline_max_bytes.saturating_mul(INLINE_BUDGET_MULTIPLE);

        for path in paths {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .ok_or(BlobError::UnsafeName("an attachment needs a file name"))?;
            check_attachment_name(&name)?;

            let (sha256, size) = self.blobs.put_file(path, self.max_attachment_bytes)?;

            let inline = size <= self.inline_max_bytes && size <= inline_budget;
            if inline {
                inline_budget -= size;
            }

            refs.push(AttachmentRef {
                name,
                size,
                sha256,
                // Guessed from the extension. Advisory only: a recipient that
                // acts on it rather than on the bytes is trusting the sender.
                mime: mime_for(path),
                inline,
            });
        }
        Ok(refs)
    }

    /// Whether this node fetches lazy attachments before they are asked for.
    #[must_use]
    pub fn prefetches(&self) -> bool {
        self.prefetch
    }

    /// Attachments of `message` that are not here yet (SPEC §8).
    ///
    /// What `prefetch = true` acts on, and what `hivemind status` would report
    /// as still outstanding.
    #[must_use]
    pub fn missing_attachments(&self, message: &Message) -> Vec<Sha256Digest> {
        message
            .attachments
            .iter()
            .map(|a| a.sha256)
            .filter(|digest| !self.blobs.has(digest))
            .collect()
    }

    /// Get an attachment onto local disk, fetching it if we do not hold it.
    ///
    /// SPEC §8: a file too large to travel with its message is fetched on
    /// first access. The fetch resumes from whatever an interrupted attempt
    /// left behind, so a laptop closed mid-transfer costs the bytes not yet
    /// received rather than all of them.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchMessage`] if the message is unknown,
    /// [`ServiceError::Blob`] if the message declares no such attachment,
    /// [`ServiceError::NoSuchPeer`] if the sender is no longer paired, or
    /// [`ServiceError::Peer`] if the fetch fails.
    pub async fn fetch_attachment(
        &self,
        message_id: Ulid,
        digest: Sha256Digest,
    ) -> Result<std::path::PathBuf, ServiceError> {
        if self.blobs.has(&digest) {
            return Ok(self.blobs.path_of(&digest));
        }

        let (_, message) = self.get(message_id)?;
        // Only an attachment this message declares. Otherwise the endpoint
        // would be a way to ask a peer for any file it happens to hold.
        if !message.attachments.iter().any(|a| a.sha256 == digest) {
            return Err(BlobError::NotFound {
                digest: digest.to_hex(),
            }
            .into());
        }

        // Our own outgoing mail: if the blob is gone it is gone, and there is
        // nobody to ask for it.
        if message.from == self.identity {
            return Err(BlobError::NotFound {
                digest: digest.to_hex(),
            }
            .into());
        }

        self.download_from(message.from, &digest).await?;
        Ok(self.blobs.path_of(&digest))
    }

    /// Fetch one blob from the peer that sent it, resuming if we can.
    async fn download_from(&self, from: NodeId, digest: &Sha256Digest) -> Result<(), ServiceError> {
        // SPEC §13.1. The blob's digest stands in for a message id here: an
        // attachment is fetched by content, and the same bytes can belong to
        // several messages.
        let span = tracing::info_span!(
            "fetch_blob",
            peer = %from.short(),
            blob = %digest.to_hex()
        );
        let _entered = span.enter();

        let certificate = self
            .certificate_of(from)
            .ok_or_else(|| ServiceError::NoSuchPeer {
                id: from.to_string(),
            })?;
        let addresses = self.peer_addresses(from)?;
        if addresses.is_empty() {
            return Err(ServiceError::NoSuchPeer {
                id: from.to_string(),
            });
        }

        let client = hivemind_net::client::PeerClient::pinned(
            &self.tls,
            hivemind_net::tls::TrustedPeers::new(vec![(from, certificate)]),
        )?;
        let path = format!("/peer/v1/blobs/{}", digest.to_hex());

        let mut last = None;
        for addr in addresses {
            let from_byte = self.blobs.partial_len(digest);
            let result = client
                .download(&addr, &path, from_byte, |chunk| {
                    self.blobs
                        .append_partial(digest, chunk)
                        .map(|_| ())
                        .map_err(|e| hivemind_net::client::ClientError::Http {
                            addr: String::new(),
                            reason: e.to_string(),
                        })
                })
                .await;

            match result {
                Ok(_) => {
                    // Verified here rather than trusted: the bytes are thrown
                    // away if they are not what the signed message named.
                    self.blobs.finish_partial(digest)?;
                    return Ok(());
                }
                Err(error) => last = Some(error),
            }
        }

        Err(last.map_or(
            ServiceError::NoSuchPeer {
                id: from.to_string(),
            },
            ServiceError::Peer,
        ))
    }

    /// Store the blobs that arrived with a message (SPEC §8).
    ///
    /// Each is matched against an attachment the *signed* message declares, so
    /// a sender cannot use a delivery to push arbitrary files into the blob
    /// store — and the bytes must hash to the digest that attachment names, so
    /// it cannot substitute different content for one it did declare.
    ///
    /// # Errors
    /// [`ServiceError::Blob`] if a part does not match what was declared.
    pub fn accept_inline_blobs(
        &self,
        message: &Message,
        blobs: Vec<(String, Vec<u8>)>,
    ) -> Result<(), ServiceError> {
        for (name, bytes) in blobs {
            let declared = message
                .attachments
                .iter()
                .find(|a| a.inline && a.sha256.to_hex() == name)
                .ok_or(BlobError::UnsafeName(
                    "a delivery carried a file the message does not declare",
                ))?;

            // Hashed before it is stored, not after: writing it first would
            // leave a file named after the wrong digest behind on every
            // rejection.
            let actual = hivemind_core::crypto::Sha256Digest::of(&bytes);
            if actual != declared.sha256 {
                return Err(BlobError::DigestMismatch {
                    expected: declared.sha256.to_hex(),
                    actual: actual.to_hex(),
                }
                .into());
            }
            self.blobs.put_bytes(&bytes)?;
        }
        Ok(())
    }

    /// The blob store, for handlers that stream attachments.
    #[must_use]
    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }
    /// The largest delivery this node will read (SPEC §8).
    ///
    /// The inline budget, plus room for the message itself. A peer configured
    /// more generously than this one gets a `413` rather than being allowed to
    /// decide how much memory this machine spends.
    #[must_use]
    pub fn max_delivery_bytes(&self) -> usize {
        let budget = self
            .inline_max_bytes
            .saturating_mul(INLINE_BUDGET_MULTIPLE)
            .saturating_add(hivemind_core::message::BODY_MAX_BYTES as u64)
            // Multipart framing, and the subject and headers alongside it.
            .saturating_add(64 * 1024);
        usize::try_from(budget).unwrap_or(usize::MAX)
    }
}

/// How many times `inline_max` one message's inline attachments may total.
///
/// SPEC §8 sets the per-file rule; this bounds the whole delivery, which the
/// recipient has to be willing to buffer.
const INLINE_BUDGET_MULTIPLE: u64 = 4;
/// A guess at a media type from the file extension.
///
/// Advisory only (SPEC §4.1). A recipient that acts on this rather than on the
/// bytes is trusting the sender, so the list is short and boring on purpose.
fn mime_for(path: &std::path::Path) -> String {
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    match extension.as_str() {
        "txt" | "log" | "toml" => "text/plain",
        "md" => "text/markdown",
        "json" => "application/json",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        _ => "application/octet-stream",
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::tests::describe;

    /// A service whose limits are small enough to test the boundaries of.
    fn service_with_limits(
        max_attachment: u64,
        inline_max: u64,
    ) -> (tempfile::TempDir, MailService) {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut node = describe(NodeId::from_certificate_der(b"this node"));
        node.max_attachment_bytes = max_attachment;
        node.inline_max_bytes = inline_max;
        let service = MailService::open(dir.path(), node, SigningKey::from_bytes(&[11u8; 32]))
            .expect("service opens");
        (dir, service)
    }

    fn file_of(dir: &tempfile::TempDir, name: &str, bytes: usize) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, vec![b'x'; bytes]).expect("write");
        path
    }

    fn draft_with(service: &MailService, attachments: Vec<std::path::PathBuf>) -> Draft {
        Draft {
            to: vec![Recipient::Node(service.identity())],
            subject: "with files".to_owned(),
            body: "see attached".to_owned(),
            kind: Kind::Message,
            in_reply_to: None,
            attachments,
        }
    }
    #[test]
    fn a_small_attachment_travels_with_the_message_and_a_large_one_does_not() {
        // SPEC §8: any single file at or below inline_max ships in the
        // delivery; larger ones ship as refs and are fetched on demand.
        let (dir, service) = service_with_limits(1_000_000, 100);
        let small = file_of(&dir, "small.txt", 100);
        let large = file_of(&dir, "large.txt", 101);

        let sent = service
            .send(draft_with(&service, vec![small, large]), SenderKind::Human)
            .expect("send")
            .message;

        assert_eq!(sent.attachments.len(), 2);
        assert!(sent.attachments[0].inline, "100 bytes is at the limit");
        assert!(!sent.attachments[1].inline, "101 is over it");
        assert_eq!(sent.attachments[0].name, "small.txt");
        assert_eq!(sent.attachments[0].size, 100);
    }

    #[test]
    fn the_inline_budget_bounds_one_delivery_without_refusing_anything() {
        // Otherwise a message with a hundred small files becomes an
        // unboundedly large request the recipient has to buffer.
        let (dir, service) = service_with_limits(1_000_000, 100);
        let budget = 100 * INLINE_BUDGET_MULTIPLE;

        // Five files of 100 bytes: four fit the budget, the fifth does not.
        let files: Vec<_> = (0..5)
            .map(|i| file_of(&dir, &format!("part{i}.bin"), 100))
            .collect();

        let sent = service
            .send(draft_with(&service, files), SenderKind::Human)
            .expect("send")
            .message;

        let inline: Vec<_> = sent.attachments.iter().filter(|a| a.inline).collect();
        assert_eq!(inline.len(), usize::try_from(budget / 100).expect("fits"));
        assert!(
            sent.attachments.iter().any(|a| !a.inline),
            "the rest should ship as refs rather than be refused"
        );
        assert_eq!(sent.attachments.len(), 5, "nothing is dropped");
    }

    #[test]
    fn an_attachment_over_the_hard_limit_is_refused() {
        let (dir, service) = service_with_limits(50, 10);
        let too_big = file_of(&dir, "huge.bin", 51);

        let error = service
            .send(draft_with(&service, vec![too_big]), SenderKind::Human)
            .expect_err("over the limit");

        assert!(matches!(
            error,
            ServiceError::Blob(BlobError::TooLarge { .. })
        ));
    }

    #[test]
    fn the_same_file_attached_twice_is_stored_once() {
        let (dir, service) = service_with_limits(1_000_000, 1_000_000);
        let path = file_of(&dir, "shared.bin", 512);

        let sent = service
            .send(
                draft_with(&service, vec![path.clone(), path]),
                SenderKind::Human,
            )
            .expect("send")
            .message;

        assert_eq!(sent.attachments.len(), 2, "both are listed");
        assert_eq!(
            sent.attachments[0].sha256, sent.attachments[1].sha256,
            "and both point at one blob"
        );
        assert!(service.blobs().has(&sent.attachments[0].sha256));
    }

    #[test]
    fn a_media_type_is_guessed_from_the_name_and_never_from_the_contents() {
        let (dir, service) = service_with_limits(1_000_000, 1_000_000);
        let png = file_of(&dir, "not-really.png", 8);

        let sent = service
            .send(draft_with(&service, vec![png]), SenderKind::Human)
            .expect("send")
            .message;

        assert_eq!(
            sent.attachments[0].mime, "image/png",
            "advisory only: acting on this rather than on the bytes is \
             trusting the sender"
        );
    }

    #[test]
    fn an_attachment_that_does_not_exist_says_so_rather_than_sending_nothing() {
        let (dir, service) = service_with_limits(1_000_000, 1_000_000);
        let missing = dir.path().join("never-written.txt");

        let error = service
            .send(draft_with(&service, vec![missing]), SenderKind::Human)
            .expect_err("no such file");

        assert!(matches!(error, ServiceError::Blob(BlobError::Io { .. })));
        assert_eq!(
            service.unread_count().expect("count"),
            0,
            "and nothing should have been sent"
        );
    }
}
