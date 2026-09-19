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

#[cfg(test)]
mod tests {
    use super::super::tests::{app_with_service, call, get, post_json};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    #[tokio::test]
    async fn a_session_registers_renews_and_closes_over_the_loopback_api() {
        // The three calls the hooks make (SPEC §9.3). One endpoint for
        // register and renew, because the hook that renews cannot know
        // whether the daemon has heard of the session.
        let (_dir, router, _service) = app_with_service();

        let (status, sessions) = call(&router, get("/api/v1/sessions")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(sessions, serde_json::json!([]));

        let (status, registered) = call(
            &router,
            post_json(
                "/api/v1/sessions/01JXT-a",
                &serde_json::json!({ "label": "hivemind" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(registered["open"], 1);

        // Renewing is the same call and must not make a second session, or a
        // long conversation would look like forty Claudes in one repo.
        let (_, registered) = call(
            &router,
            post_json(
                "/api/v1/sessions/01JXT-a",
                &serde_json::json!({ "label": "hivemind" }),
            ),
        )
        .await;
        assert_eq!(registered["open"], 1);

        let (_, sessions) = call(&router, get("/api/v1/sessions")).await;
        assert_eq!(sessions.as_array().expect("array").len(), 1);
        assert_eq!(sessions[0]["label"], "hivemind");
        assert!(sessions[0]["last_seen"].is_string());

        let request = Request::builder()
            .method("DELETE")
            .uri("/api/v1/sessions/01JXT-a")
            .body(Body::empty())
            .expect("request");
        let response = router.clone().oneshot(request).await.expect("responds");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let (_, sessions) = call(&router, get("/api/v1/sessions")).await;
        assert_eq!(sessions, serde_json::json!([]));
    }

    #[tokio::test]
    async fn closing_a_session_nobody_registered_is_not_an_error() {
        // `SessionEnd` is a courtesy; expiry is what makes the list true. A
        // daemon that restarted mid-conversation never saw the start, and a
        // hook must never fail because of hivemind.
        let (_dir, router, _service) = app_with_service();

        let request = Request::builder()
            .method("DELETE")
            .uri("/api/v1/sessions/01JXT-never-seen")
            .body(Body::empty())
            .expect("request");
        let response = router.oneshot(request).await.expect("responds");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn a_session_without_a_label_is_refused_rather_than_listed_blank() {
        // The label is the whole of what a session says. A blank one would
        // show up in somebody else's peer list as "online, 1 session: ".
        let (_dir, router, _service) = app_with_service();

        let (status, problem) = call(
            &router,
            post_json(
                "/api/v1/sessions/01JXT-a",
                &serde_json::json!({ "label": "   " }),
            ),
        )
        .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(problem["type"], "/problems/invalid-message");
    }
}
