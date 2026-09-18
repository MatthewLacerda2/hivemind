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
    /// The caller is not paired with this node (SPEC §6.2).
    NotPaired,
    /// What the caller claimed does not match the certificate it presented.
    IdentityMismatch,
    /// The message's signature did not verify.
    BadSignature,
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
            Self::IdentityMismatch => "identity-mismatch",
            Self::BadSignature => "bad-signature",
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
            Self::IdentityMismatch => "Identity does not match the certificate",
            Self::BadSignature => "Signature does not verify",
            Self::Internal => "Internal error",
        }
    }

    /// The HTTP status that goes with it.
    #[must_use]
    pub fn status(self) -> StatusCode {
        match self {
            Self::MessageNotFound => StatusCode::NOT_FOUND,
            Self::InvalidMessage | Self::NoRecipients => StatusCode::UNPROCESSABLE_ENTITY,
            // SPEC §6.2 names this status explicitly.
            Self::NotPaired => StatusCode::FORBIDDEN,
            Self::IdentityMismatch | Self::BadSignature => StatusCode::BAD_REQUEST,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Every slug, for the generated documentation.
    pub const ALL: [Self; 7] = [
        Self::MessageNotFound,
        Self::InvalidMessage,
        Self::NoRecipients,
        Self::NotPaired,
        Self::IdentityMismatch,
        Self::BadSignature,
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
            ServiceError::NoSuchPeer { .. } => ProblemType::NotPaired,
            ServiceError::Store(_)
            | ServiceError::Index(_)
            | ServiceError::Canonical(_)
            | ServiceError::PeerBook(_)
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
