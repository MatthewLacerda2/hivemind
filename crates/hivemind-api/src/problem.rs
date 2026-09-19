//! RFC 9457 `application/problem+json` responses.
//!
//! Every failure the API can return is one variant of one enum, so the stable
//! `type` slugs and their documentation cannot fall out of sync (SPEC §7.3).
//!
//! Variants are added as the milestone that can return them lands. A slug for
//! an endpoint that does not exist yet would be a promise the code does not
//! keep.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

use hivemind_core::blobs::BlobError;

use crate::service::ServiceError;

/// The stable identity of a failure.
///
/// The slug is the API's contract — it is what a client matches on, and it does
/// not change even if the wording of `title` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemType {
    /// No message with that id.
    MessageNotFound,
    /// The message broke a limit in SPEC §4.1.
    InvalidMessage,
    /// The message was not addressed to anybody.
    NoRecipients,
    /// The caller has not proved the group key to this node (SPEC §6.2).
    NotPaired,
    /// This node is already in a group, and the request did not say to leave
    /// it (SPEC §6.2).
    AlreadyInGroup,
    /// What was pasted is not a group code (SPEC §6.2).
    InvalidGroupCode,
    /// What the caller claimed does not match the certificate it presented.
    IdentityMismatch,
    /// The message's signature did not verify.
    BadSignature,
    /// A host we were asked to join could not be reached, or refused us.
    PeerUnreachable,
    /// No attachment with that digest, here or at the sender.
    BlobNotFound,
    /// The attachment is larger than this node accepts (SPEC §6.3).
    BlobTooLarge,
    /// The attachment's name is not a name (SPEC §6.3).
    UnsafeAttachmentName,
    /// Something went wrong that is not the caller's fault.
    Internal,
}

impl ProblemType {
    /// The stable slug, used as the `type` member.
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            Self::MessageNotFound => "message-not-found",
            Self::InvalidMessage => "invalid-message",
            Self::NoRecipients => "no-recipients",
            Self::NotPaired => "not-paired",
            Self::AlreadyInGroup => "already-in-group",
            Self::InvalidGroupCode => "invalid-group-code",
            Self::IdentityMismatch => "identity-mismatch",
            Self::BadSignature => "bad-signature",
            Self::PeerUnreachable => "peer-unreachable",
            Self::BlobNotFound => "blob-not-found",
            Self::BlobTooLarge => "blob-too-large",
            Self::UnsafeAttachmentName => "unsafe-attachment-name",
            Self::Internal => "internal",
        }
    }

    /// A short, human-readable summary that does not change per occurrence.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::MessageNotFound => "No such message",
            Self::InvalidMessage => "Message is not acceptable",
            Self::NoRecipients => "Message has no recipients",
            Self::NotPaired => "Not paired",
            Self::AlreadyInGroup => "Already in a group",
            Self::InvalidGroupCode => "Not a group code",
            Self::IdentityMismatch => "Identity does not match the certificate",
            Self::BadSignature => "Signature does not verify",
            Self::PeerUnreachable => "Peer could not be reached",
            Self::BlobNotFound => "No such attachment",
            Self::BlobTooLarge => "Attachment is too large",
            Self::UnsafeAttachmentName => "Attachment name is not a file name",
            Self::Internal => "Internal error",
        }
    }

    /// The HTTP status that goes with it.
    #[must_use]
    pub fn status(self) -> StatusCode {
        match self {
            Self::MessageNotFound | Self::BlobNotFound => StatusCode::NOT_FOUND,
            // 413 rather than 422: the request was well formed, it is the
            // thing it carries that is too big.
            Self::BlobTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::UnsafeAttachmentName
            | Self::InvalidMessage
            | Self::NoRecipients
            | Self::InvalidGroupCode => StatusCode::UNPROCESSABLE_ENTITY,
            // The request is fine; it conflicts with the group already held.
            Self::AlreadyInGroup => StatusCode::CONFLICT,
            // SPEC §6.2 names this status explicitly.
            Self::NotPaired => StatusCode::FORBIDDEN,
            Self::IdentityMismatch | Self::BadSignature => StatusCode::BAD_REQUEST,
            // Not this node's fault and not the caller's: the machine it
            // asked us to talk to did not answer.
            Self::PeerUnreachable => StatusCode::BAD_GATEWAY,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Every slug, for the generated documentation.
    pub const ALL: [Self; 13] = [
        Self::MessageNotFound,
        Self::InvalidMessage,
        Self::NoRecipients,
        Self::NotPaired,
        Self::AlreadyInGroup,
        Self::InvalidGroupCode,
        Self::IdentityMismatch,
        Self::BadSignature,
        Self::PeerUnreachable,
        Self::BlobNotFound,
        Self::BlobTooLarge,
        Self::UnsafeAttachmentName,
        Self::Internal,
    ];
}

/// An RFC 9457 problem document.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Problem {
    /// A relative URI reference identifying the problem type, stable across
    /// releases: `/problems/<slug>`.
    #[schema(example = "/problems/message-not-found")]
    pub r#type: String,
    /// A short summary of the problem type.
    pub title: String,
    /// The HTTP status code.
    pub status: u16,
    /// What went wrong with *this* request.
    pub detail: String,
}

impl Problem {
    /// Build a problem document.
    #[must_use]
    pub fn new(kind: ProblemType, detail: impl Into<String>) -> Self {
        Self {
            r#type: format!("/problems/{}", kind.slug()),
            title: kind.title().to_owned(),
            status: kind.status().as_u16(),
            detail: detail.into(),
        }
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut response = (status, Json(&self)).into_response();
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/problem+json"),
        );
        response
    }
}

impl From<ServiceError> for Problem {
    fn from(error: ServiceError) -> Self {
        // The mapping lives here rather than at each call site so that a new
        // service error cannot reach a client as an unlabelled 500.
        let kind = match &error {
            ServiceError::NoSuchMessage { .. } => ProblemType::MessageNotFound,
            ServiceError::Invalid(_) => ProblemType::InvalidMessage,
            ServiceError::NoRecipients => ProblemType::NoRecipients,
            ServiceError::BadSignature => ProblemType::BadSignature,
            ServiceError::NoSuchPeer { .. } | ServiceError::NotInGroup(_) => ProblemType::NotPaired,
            ServiceError::Peer(_) => ProblemType::PeerUnreachable,
            ServiceError::Blob(BlobError::NotFound { .. }) => ProblemType::BlobNotFound,
            ServiceError::Blob(BlobError::TooLarge { .. }) => ProblemType::BlobTooLarge,
            ServiceError::Blob(BlobError::UnsafeName(_)) => ProblemType::UnsafeAttachmentName,
            // A digest mismatch or a failed write is this node's problem, not
            // the caller's, and says nothing useful to them.
            ServiceError::Blob(BlobError::DigestMismatch { .. } | BlobError::Io { .. }) => {
                ProblemType::Internal
            }
            ServiceError::IdentityMismatch(_) => ProblemType::IdentityMismatch,
            ServiceError::AlreadyInGroup => ProblemType::AlreadyInGroup,
            ServiceError::Group(hivemind_core::group::GroupError::InvalidCode) => {
                ProblemType::InvalidGroupCode
            }
            ServiceError::Store(_)
            | ServiceError::Index(_)
            | ServiceError::Canonical(_)
            | ServiceError::PeerBook(_)
            | ServiceError::Group(_)
            | ServiceError::Unavailable => ProblemType::Internal,
        };

        if kind == ProblemType::Internal {
            // The caller gets a generic message; the operator gets the detail.
            tracing::error!(error = %error, "request failed");
            return Self::new(kind, "something went wrong on this node");
        }
        Self::new(kind, error.to_string())
    }
}

impl IntoResponse for ServiceError {
    fn into_response(self) -> Response {
        Problem::from(self).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_problem_type_is_in_the_protocol_document() {
        // The slugs are the API's contract (SPEC §7.3). A new failure reaching
        // clients undocumented is the drift this catches; the table used to
        // carry a `TODO(M1)` instead, four milestones after M1 shipped.
        let doc = include_str!("../../../docs/protocol.md");

        for kind in ProblemType::ALL {
            let slug = format!("/problems/{}", kind.slug());
            assert!(
                doc.contains(&slug),
                "{slug} is missing from docs/protocol.md — add a row for it"
            );
            assert!(
                doc.contains(kind.title()),
                "{:?}'s title is missing from docs/protocol.md",
                kind.slug()
            );
        }
    }

    #[test]
    fn the_document_lists_nothing_that_is_not_a_problem_type() {
        // The other direction: a row for a slug the code cannot produce sends
        // a client matching on something that will never arrive.
        let doc = include_str!("../../../docs/protocol.md");
        let known: Vec<String> = ProblemType::ALL
            .iter()
            .map(|kind| format!("/problems/{}", kind.slug()))
            .collect();

        for line in doc.lines() {
            let Some(start) = line.find("`/problems/") else {
                continue;
            };
            let rest = &line[start + 1..];
            let Some(end) = rest.find('`') else { continue };
            let slug = &rest[..end];
            assert!(
                known.iter().any(|k| k == slug),
                "docs/protocol.md lists {slug}, which no ProblemType produces"
            );
        }
    }

    #[test]
    fn a_problem_carries_its_status_in_the_body_and_the_response() {
        // RFC 9457 says `status` duplicates the HTTP status. A client reading
        // one and getting the other would be right to be confused.
        for kind in ProblemType::ALL {
            let problem = Problem::new(kind, "why");
            assert_eq!(problem.status, kind.status().as_u16());
            assert_eq!(problem.r#type, format!("/problems/{}", kind.slug()));
        }
    }

    #[test]
    fn slugs_are_unique_and_stable_looking() {
        let mut slugs: Vec<&str> = ProblemType::ALL.iter().map(|k| k.slug()).collect();
        slugs.sort_unstable();
        let count = slugs.len();
        slugs.dedup();
        assert_eq!(slugs.len(), count, "two problem types share a slug");

        for slug in &slugs {
            assert!(
                slug.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "{slug} is not a stable kebab-case slug"
            );
        }
    }
}
