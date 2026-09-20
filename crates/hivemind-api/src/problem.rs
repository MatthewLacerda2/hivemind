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

/// Declare the failures the API can return, once.
///
/// The enum, the slugs, the titles and [`ProblemType::ALL`] all come out of
/// this one list, so no two of them can disagree.
///
/// `ALL` used to be written by hand beside the enum, and it was the one part of
/// the enum the compiler did not hold: a new variant had to be added to `slug`,
/// `title` and `status`, because those are exhaustive matches, and could be
/// left out of the array in silence. Everything that documents a failure walks
/// `ALL` — the `docs/protocol.md` table check, the `docs/openapi.json`
/// generator, both uniqueness checks — so a variant missing from it reached
/// clients undocumented with every one of them green (#106). Measured, not
/// reasoned: a throwaway variant with its own slug, title and status passed the
/// whole suite and `just openapi-check`.
///
/// `status` stays a hand-written match below, because its arguments are about
/// groups of variants rather than about one — why 400 rather than 422, why 413
/// rather than 422 — and the compiler already refuses a variant it does not
/// answer for.
macro_rules! problem_types {
    (
        $(
            $(#[doc = $doc:literal])+
            $variant:ident => $slug:literal, $title:literal;
        )+
    ) => {
        /// The stable identity of a failure.
        ///
        /// The slug is the API's contract — it is what a client matches on, and
        /// it does not change even if the wording of `title` does.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum ProblemType {
            $(
                $(#[doc = $doc])+
                $variant,
            )+
        }

        impl ProblemType {
            /// Every problem type, for the generated documentation.
            ///
            /// The length is counted from the same list rather than written, so
            /// it cannot be the thing that drifts either.
            pub const ALL: [Self; [$(stringify!($variant)),+].len()] =
                [$(Self::$variant),+];

            /// The stable slug, used as the `type` member.
            #[must_use]
            pub fn slug(self) -> &'static str {
                match self {
                    $(Self::$variant => $slug,)+
                }
            }

            /// A short, human-readable summary that does not change per
            /// occurrence.
            #[must_use]
            pub fn title(self) -> &'static str {
                match self {
                    $(Self::$variant => $title,)+
                }
            }
        }
    };
}

problem_types! {
    /// No message with that id.
    MessageNotFound => "message-not-found", "No such message";
    /// What was typed names more than one message, and guessing which is
    /// worse than asking again (SPEC §10).
    AmbiguousId => "ambiguous-id", "Id matches more than one message";
    /// The query string asked something that makes no sense — an unknown
    /// parameter, or a value that is not one of the allowed ones (SPEC §7.1).
    InvalidQuery => "invalid-query", "Query is not one this endpoint understands";
    /// The message broke a limit in SPEC §4.1.
    InvalidMessage => "invalid-message", "Message is not acceptable";
    /// The message was not addressed to anybody.
    NoRecipients => "no-recipients", "Message has no recipients";
    /// The caller has not proved the group key to this node (SPEC §6.2).
    NotPaired => "not-paired", "Not paired";
    /// This node is already in a group, and the request did not say to leave
    /// it (SPEC §6.2).
    AlreadyInGroup => "already-in-group", "Already in a group";
    /// What was pasted is not a group code (SPEC §6.2).
    InvalidGroupCode => "invalid-group-code", "Not a group code";
    /// What the caller claimed does not match the certificate it presented.
    IdentityMismatch => "identity-mismatch", "Identity does not match the certificate";
    /// The message's signature did not verify.
    BadSignature => "bad-signature", "Signature does not verify";
    /// A host we were asked to join could not be reached, or refused us.
    PeerUnreachable => "peer-unreachable", "Peer could not be reached";
    /// No attachment with that digest, here or at the sender.
    BlobNotFound => "blob-not-found", "No such attachment";
    /// The attachment is larger than this node accepts (SPEC §6.3).
    BlobTooLarge => "blob-too-large", "Attachment is too large";
    /// The attachment's name is not a name (SPEC §6.3).
    UnsafeAttachmentName => "unsafe-attachment-name", "Attachment name is not a file name";
    /// The peer is known; the address asked about is not one of the ways to
    /// reach it (SPEC §10).
    AddrNotFound => "addr-not-found", "No such address";
    /// Something went wrong that is not the caller's fault.
    Internal => "internal", "Internal error";
}

impl ProblemType {
    /// The HTTP status that goes with it.
    #[must_use]
    pub fn status(self) -> StatusCode {
        match self {
            Self::MessageNotFound | Self::BlobNotFound | Self::AddrNotFound => {
                StatusCode::NOT_FOUND
            }
            // 413 rather than 422: the request was well formed, it is the
            // thing it carries that is too big.
            Self::BlobTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::UnsafeAttachmentName
            | Self::InvalidMessage
            | Self::NoRecipients
            | Self::InvalidGroupCode
            // Understood perfectly; it just names several things. The caller
            // has to choose, which is why the detail lists what it matched.
            | Self::AmbiguousId => StatusCode::UNPROCESSABLE_ENTITY,
            // The request is fine; it conflicts with the group already held.
            Self::AlreadyInGroup => StatusCode::CONFLICT,
            // SPEC §6.2 names this status explicitly.
            Self::NotPaired => StatusCode::FORBIDDEN,
            // 400 rather than 422: the request itself is malformed, which is
            // what a caller who typed `mailbox=` instead of `box=` needs told.
            Self::IdentityMismatch | Self::BadSignature | Self::InvalidQuery => {
                StatusCode::BAD_REQUEST
            }
            // Not this node's fault and not the caller's: the machine it
            // asked us to talk to did not answer.
            Self::PeerUnreachable => StatusCode::BAD_GATEWAY,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
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
            ServiceError::NoSuchMessage { .. } | ServiceError::NoSuchMessageTail { .. } => {
                ProblemType::MessageNotFound
            }
            ServiceError::AmbiguousMessage { .. } => ProblemType::AmbiguousId,
            ServiceError::Invalid(_) => ProblemType::InvalidMessage,
            ServiceError::NoRecipients => ProblemType::NoRecipients,
            ServiceError::BadSignature => ProblemType::BadSignature,
            ServiceError::NoSuchPeer { .. } | ServiceError::NotInGroup(_) => ProblemType::NotPaired,
            ServiceError::NoSuchAddr { .. } => ProblemType::AddrNotFound,
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

    /// One row of the error table in `docs/protocol.md`, which is
    /// ``| `/problems/<slug>` | <title> | <status> |``.
    struct Row<'a> {
        slug: &'a str,
        title: &'a str,
        status: &'a str,
    }

    /// The table's rows, in the order the document lists them.
    ///
    /// The check reads the row rather than searching the whole document,
    /// because `doc.contains(needle)` cannot fail for an empty needle and
    /// passes for one that lands anywhere — inside another word, or inside a
    /// longer title the table has since drifted away from (#97).
    fn documented_problems(doc: &str) -> Vec<Row<'_>> {
        doc.lines()
            .filter_map(|line| {
                let mut cells = line.split('|').map(str::trim);
                // A row opens with the delimiter, so the first cell is empty.
                cells.next()?;
                let slug = cells.next()?.trim_matches('`');
                let title = cells.next()?;
                let status = cells.next()?;
                slug.starts_with("/problems/").then_some(Row {
                    slug,
                    title,
                    status,
                })
            })
            .collect()
    }

    #[test]
    fn every_problem_type_has_a_row_carrying_its_title_and_status() {
        // The slugs are the API's contract (SPEC §7.3). A new failure reaching
        // clients undocumented is the drift this catches; the table used to
        // carry a `TODO(M1)` instead, four milestones after M1 shipped.
        let doc = include_str!("../../../docs/protocol.md");
        let rows = documented_problems(doc);

        for kind in ProblemType::ALL {
            let slug = format!("/problems/{}", kind.slug());
            let matching: Vec<&Row<'_>> = rows.iter().filter(|row| row.slug == slug).collect();
            assert_eq!(
                matching.len(),
                1,
                "docs/protocol.md should have exactly one row for {slug}"
            );

            let row = matching[0];
            assert_eq!(row.title, kind.title(), "{slug}'s title has drifted");
            // The status column was documented and unchecked until #97. A row
            // promising a 404 where the code answers 500 misleads a client as
            // surely as a missing row does.
            assert_eq!(
                row.status,
                kind.status().as_u16().to_string(),
                "{slug}'s status has drifted"
            );
        }
    }

    #[test]
    fn a_problem_type_has_a_title_and_a_slug_to_put_in_the_table() {
        // The rule is on the code, not on the document: a blank title matches
        // a blank cell, so the row check alone would still admit one. And
        // `title` is what a client shows a person when it understands nothing
        // else about the failure.
        for kind in ProblemType::ALL {
            assert!(
                !kind.title().trim().is_empty(),
                "{} has no title",
                kind.slug()
            );
            // A delimiter would split the cell the title has to sit in, so the
            // row check would then fail for a reason nobody could read.
            assert!(
                !kind.title().contains('|'),
                "{}'s title cannot contain a table delimiter",
                kind.slug()
            );
            assert!(
                !kind.slug().is_empty(),
                "{kind:?} has no slug, so its `type` member would be /problems/"
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
