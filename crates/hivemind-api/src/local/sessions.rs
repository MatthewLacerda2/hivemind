//! The session endpoints of the loopback router (SPEC §7.1, §9.3).
//!
//! A child of `local` for the same reasons `group` is: they are loopback
//! routes like the rest, and `local.rs` is past the size the gate allows it
//! to grow.
//!
//! These are written by the Claude Code hooks on **this** machine and by
//! nothing else. Loopback-only is the whole of the authorisation (SPEC §6.3):
//! a process that can reach `127.0.0.1:8401` is already running as the user,
//! and a session register is not a thing worth a second gate that machine
//! would then have to keep a secret for.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;
use crate::problem::Problem;

/// One session open on this machine.
#[derive(Debug, Serialize, ToSchema)]
pub struct SessionSummary {
    /// What it is working on — the basename of its working directory.
    pub label: String,
    /// When it last showed a sign of life, RFC 3339.
    pub last_seen: String,
}

/// What a registration says about itself.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RegisterSession {
    /// What it is working on. The hook sends the basename of its `cwd`.
    pub label: String,
}

/// What a registration answers.
#[derive(Debug, Serialize, ToSchema)]
pub struct Registered {
    /// How many sessions are open now, this one included.
    pub open: usize,
}

#[utoipa::path(
    get, path = "/api/v1/sessions",
    responses((status = 200, body = Vec<SessionSummary>)),
    tag = "sessions"
)]
pub(crate) async fn list_sessions(State(service): State<AppState>) -> Json<Vec<SessionSummary>> {
    Json(
        service
            .open_sessions()
            .into_iter()
            .map(|session| SessionSummary {
                label: session.label,
                last_seen: session.last_seen.to_rfc3339(),
            })
            .collect(),
    )
}

/// Register a session, or renew one already known (SPEC §9.3).
///
/// One call for both, because the hook that renews cannot know whether the
/// daemon has heard of it — a daemon restarted mid-conversation missed the
/// `SessionStart`, and the next prompt is the first it learns of a session
/// that is plainly alive.
#[utoipa::path(
    post, path = "/api/v1/sessions/{id}",
    request_body = RegisterSession,
    params(("id" = String, Path, description = "The session id Claude Code gives its hooks")),
    responses((status = 200, body = Registered), (status = 422, body = Problem)),
    tag = "sessions"
)]
pub(crate) async fn register_session(
    State(service): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<RegisterSession>,
) -> Result<Json<Registered>, Problem> {
    let label = request.label.trim();
    if label.is_empty() {
        return Err(Problem::new(
            crate::problem::ProblemType::InvalidMessage,
            "a session needs a label; the hook sends the basename of its working directory",
        ));
    }

    service.register_session(&id, label);
    Ok(Json(Registered {
        open: service.open_sessions().len(),
    }))
}

/// Close a session (SPEC §9.3).
///
/// `204` whether or not there was one. `SessionEnd` is a courtesy and expiry
/// is what makes the list true, so "there was nothing to close" is the
/// caller's request already satisfied rather than a mistake to report.
#[utoipa::path(
    delete, path = "/api/v1/sessions/{id}",
    params(("id" = String, Path, description = "The session id Claude Code gives its hooks")),
    responses((status = 204, description = "Closed, or there was nothing to close")),
    tag = "sessions"
)]
pub(crate) async fn end_session(
    State(service): State<AppState>,
    Path(id): Path<String>,
) -> axum::http::StatusCode {
    service.end_session(&id);
    axum::http::StatusCode::NO_CONTENT
}
