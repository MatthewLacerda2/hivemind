//! The group endpoints of the loopback router (SPEC §7.1, §6.2).
//!
//! A child of `local` because they are loopback routes like the rest, and a
//! file of their own because `local.rs` is past the size the gate allows to
//! grow.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;
use crate::problem::Problem;

/// Which group this node is in. Never the key.
#[derive(Debug, Serialize, ToSchema)]
pub struct GroupSummary {
    /// Whether this node is in a group at all.
    pub in_group: bool,
    /// When it joined or created it.
    pub joined_at: Option<String>,
    /// How many other nodes have proved the key to this one.
    pub members: usize,
}

/// A new group's code, shown once.
#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedGroup {
    /// What every other machine pastes into `hivemind pair`.
    #[schema(example = "hm-aaaq-eaye-auda-ocaj-bifq-ydio-b4")]
    pub code: String,
}

/// Make a new group.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct CreateGroup {
    /// Leave the group this node is in for a new one. This is how a group
    /// rotates its key; without it, a node already in a group is refused.
    #[serde(default)]
    pub replace: bool,
}

/// Join the group a code belongs to.
#[derive(Debug, Deserialize, ToSchema)]
pub struct JoinGroup {
    /// The code, as pasted. Case, the `hm-` prefix and hyphens do not matter.
    #[schema(example = "hm-aaaq-eaye-auda-ocaj-bifq-ydio-b4")]
    pub code: String,
    /// Leave a different group this node is already in.
    #[serde(default)]
    pub replace: bool,
}

fn summary(service: &AppState) -> Result<GroupSummary, Problem> {
    let status = service.group_status()?;
    Ok(GroupSummary {
        in_group: status.joined_at.is_some(),
        joined_at: status.joined_at.map(|t| t.to_rfc3339()),
        members: status.members,
    })
}

/// Greet everything seen while this node had no key, without holding up the
/// answer: the person who pasted the code wants to know it was taken, and
/// the greetings are several round trips.
fn greet_in_background(service: &AppState) {
    let service = Arc::clone(service);
    tokio::spawn(async move { service.greet_everyone_seen().await });
}

#[utoipa::path(
    get, path = "/api/v1/group",
    responses((status = 200, body = GroupSummary), (status = 500, body = Problem)),
    tag = "group"
)]
pub(crate) async fn status(State(service): State<AppState>) -> Result<Json<GroupSummary>, Problem> {
    Ok(Json(summary(&service)?))
}

#[utoipa::path(
    post, path = "/api/v1/group/create",
    request_body = CreateGroup,
    responses(
        (status = 200, body = CreatedGroup),
        (status = 409, body = Problem),
        (status = 500, body = Problem)
    ),
    tag = "group"
)]
pub(crate) async fn create(
    State(service): State<AppState>,
    request: Option<Json<CreateGroup>>,
) -> Result<Json<CreatedGroup>, Problem> {
    let Json(request) = request.unwrap_or_default();
    let key = service.create_group(request.replace)?;
    greet_in_background(&service);
    Ok(Json(CreatedGroup { code: key.code() }))
}

#[utoipa::path(
    post, path = "/api/v1/group/join",
    request_body = JoinGroup,
    responses(
        (status = 200, body = GroupSummary),
        (status = 409, body = Problem),
        (status = 422, body = Problem)
    ),
    tag = "group"
)]
pub(crate) async fn join(
    State(service): State<AppState>,
    Json(request): Json<JoinGroup>,
) -> Result<Json<GroupSummary>, Problem> {
    service.join_group(&request.code, request.replace)?;
    greet_in_background(&service);
    Ok(Json(summary(&service)?))
}
