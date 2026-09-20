//! `utoipa` document assembly.
//!
//! The generated `openapi.json` is checked into `docs/` and CI fails if it
//! drifts from the code (SPEC §7). Regenerate with
//! `hivemind openapi --stdout > docs/openapi.json`.

use utoipa::OpenApi;

use crate::local;
use crate::problem::{Problem, ProblemType};

/// The local API's `OpenAPI` document (SPEC §7.1).
#[derive(Debug, OpenApi)]
#[openapi(
    info(
        title = "hivemind local API",
        description = "The loopback API on 127.0.0.1:8401. No authentication: \
                       reachability is the authorization, and the listener \
                       refuses to bind anywhere else.",
        license(name = "Apache-2.0 OR MIT")
    ),
    paths(
        local::healthz,
        local::me,
        local::list_peers,
        local::join_peer,
        local::refresh_peers,
        local::remove_peer,
        local::forget_addr,
        local::group::status,
        local::group::create,
        local::group::join,
        local::list_messages,
        local::send_message,
        local::get_message,
        local::reply_to_message,
        local::mark_read,
        local::get_attachment,
        local::threads::list_threads,
        local::threads::get_thread,
        local::events,
        local::sessions::list_sessions,
        local::sessions::register_session,
        local::sessions::end_session,
    ),
    components(schemas(
        local::Me,
        local::PeerSummary,
        local::JoinRequest,
        local::ForgetAddrRequest,
        local::Refreshed,
        local::group::GroupSummary,
        local::group::CreatedGroup,
        local::group::CreateGroup,
        local::group::JoinGroup,
        local::MessageSummary,
        local::MessageBody,
        local::Attachment,
        local::SendRequest,
        local::ReplyRequest,
        local::Accepted,
        local::threads::ConversationSummary,
        local::sessions::SessionSummary,
        local::sessions::RegisterSession,
        local::sessions::Registered,
        Problem,
    )),
    tags(
        (name = "messages", description = "Reading and sending mail"),
        (name = "peers", description = "Members, nodes seen, and where they are"),
        (name = "group", description = "The group this node is in (SPEC §6.2)"),
        (name = "node", description = "This node"),
        (name = "sessions", description = "Claude Code sessions open on this machine (SPEC §9.3)")
    )
)]
pub struct ApiDoc;

impl ApiDoc {
    /// The document as pretty JSON, which is what is checked into `docs/`.
    ///
    /// # Errors
    /// Returns an error if the document cannot be serialised.
    pub fn to_json() -> Result<String, serde_json::Error> {
        let mut doc = Self::openapi();

        // The problem slugs are part of the contract (SPEC §7.3), and listing
        // them here means the one Rust enum generates the documentation rather
        // than a hand-kept table going stale beside it.
        let slugs = ProblemType::ALL
            .iter()
            .map(|p| {
                format!(
                    "- `/problems/{}` — {} ({})",
                    p.slug(),
                    p.title(),
                    p.status().as_u16()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(description) = doc.info.description.as_mut() {
            description.push_str(
                "\n\n## Problem types\n\nErrors are RFC 9457 \
                                  `application/problem+json`. The `type` member is stable:\n\n",
            );
            description.push_str(&slugs);
        }

        serde_json::to_string_pretty(&doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_documented_path_is_a_real_route() {
        // A path in the document that the router does not serve is a lie the
        // Swagger UI tells (SPEC §7).
        let doc = ApiDoc::openapi();
        let documented: Vec<&String> = doc.paths.paths.keys().collect();
        assert!(!documented.is_empty());
        for path in &documented {
            assert!(
                path.starts_with("/api/v1/") || path.as_str() == "/healthz",
                "{path} is not a path this router serves"
            );
        }
    }

    #[test]
    fn the_document_covers_every_endpoint_m1_serves() {
        let doc = ApiDoc::openapi();
        for expected in [
            "/healthz",
            "/api/v1/me",
            "/api/v1/messages",
            "/api/v1/messages/{id}",
            "/api/v1/messages/{id}/reply",
            "/api/v1/messages/{id}/read",
            "/api/v1/threads",
            "/api/v1/threads/{thread_id}",
            "/api/v1/events",
        ] {
            assert!(
                doc.paths.paths.contains_key(expected),
                "{expected} is served but not documented"
            );
        }
    }

    #[test]
    fn every_problem_slug_appears_in_the_document() {
        let json = ApiDoc::to_json().expect("serialise");
        for problem in ProblemType::ALL {
            assert!(
                json.contains(problem.slug()),
                "{} is missing from the document",
                problem.slug()
            );
        }
    }

    #[test]
    fn problem_slugs_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for problem in ProblemType::ALL {
            assert!(
                seen.insert(problem.slug()),
                "duplicate slug {}",
                problem.slug()
            );
        }
    }
}
