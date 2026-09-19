//! Presence: `POST /peer/v1/hello` (SPEC §5.5, §7.2).
//!
//! A hello is the handshake's periodic cousin. The handshake runs once per
//! peer and decides whether there is a peer at all; a hello runs every
//! `presence_interval` and says *still here, still in the group, and here is
//! everybody else I know*.
//!
//! Three things ride on it that have nowhere else to go:
//!
//! - **The proof.** Membership is possession of the group key (ADR 0013), and
//!   a proof offered once at pairing time can never be withdrawn. Asking for a
//!   fresh one every minute is what gives `group create --replace` an effect
//!   on a node that is already pinned in `peers.toml`.
//! - **The peer list.** SPEC §5.4 reserved gossip for later; it arrives here
//!   rather than on the handshake because presence runs always and a handshake
//!   runs once, so a node that joined through any one member has the whole
//!   group inside a round.
//! - **The sessions**, which are #52's to fill. Until then the list is empty
//!   and the field exists so that filling it is not a protocol change.

use std::sync::Arc;

use axum::extract::State;
use axum::{Extension, Json};
use hivemind_net::listener::CallerIdentity;
use serde::{Deserialize, Serialize};

use super::{Proof, callback_address};
use crate::problem::Problem;
use crate::service::MailService;

/// One node as another describes it (SPEC §5.4).
///
/// Nothing here is believed beyond "try this address". The certificate is what
/// gets pinned and the group key is what admits, so the worst a lying member
/// can do is waste a connection attempt on an address that answers as somebody
/// else — or as nobody.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerNote {
    /// The node's fingerprint, in display form.
    pub id: String,
    /// What it calls itself.
    pub name: String,
    /// Who owns it, if it said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Where the sender reaches it, as `host:port`, best guess first.
    #[serde(default)]
    pub addrs: Vec<String>,
}

/// A Claude Code session open on the sending machine (SPEC §9.3).
///
/// Presence, not address: mail is delivered to the node and any session may
/// read it (ADR 0003). This says only how many and in what.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionNote {
    /// The session's id, as Claude Code passes it to the hooks.
    pub id: String,
    /// What it is working on — the basename of its working directory.
    pub label: String,
}

/// What presence says, in both directions (SPEC §5.5).
///
/// The answer to a hello is a hello, so one request tells each side about the
/// other. That is what lets presence work through a NAT that only one of the
/// two can traverse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    /// The sender's node id, as a string. Advisory: the certificate is the
    /// identity, and a mismatch is rejected.
    pub id: String,
    /// Its display name, which may have changed since the handshake.
    pub name: String,
    /// The owner it claims. Unverified, and never a security boundary
    /// (ADR 0003).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// The version it is running.
    pub version: String,
    /// The host it can be called back on.
    pub callback_host: String,
    /// The port its peer listener is on.
    pub callback_port: u16,
    /// Fresh proof that the sender still holds the group key (SPEC §6.2).
    ///
    /// Absent when it is in no group, which the receiver refuses exactly as it
    /// refuses a wrong one — and refusing here is what retires a member whose
    /// key was rotated away.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<Proof>,
    /// Everybody the sender knows (SPEC §5.4).
    #[serde(default)]
    pub peers: Vec<PeerNote>,
    /// The sessions open on the sending machine (SPEC §9.3). Empty until #52.
    #[serde(default)]
    pub sessions: Vec<SessionNote>,
    /// Nodes the sender has just heard from, as a hint (SPEC §5.5).
    ///
    /// Never believed: the receiver says hello to each itself and marks it
    /// online on the answer, or does not. A hint costs one request and saves
    /// a peer a whole interval of looking absent to everyone but the one node
    /// it happened to reach first.
    #[serde(default)]
    pub up: Vec<String>,
}

/// Answer a hello from a member, and say hello back.
///
/// Unlike the handshake this is not open to strangers: a node that cannot
/// prove the group key is answered `403 not_paired` *and dropped from*
/// `peers.toml` if it was in it. SPEC §6.2.4 says a rotated-out member stops
/// receiving mail, and leaving it pinned but flagged would be a second state
/// to keep in step with the first.
pub(super) async fn hello(
    State(service): State<Arc<MailService>>,
    Extension(caller): Extension<CallerIdentity>,
    Json(request): Json<Hello>,
) -> Result<Json<Hello>, Problem> {
    let span = tracing::info_span!("hello", peer = %caller.node_id.short());
    let _entered = span.enter();

    // The same check the handshake makes, for the same reason: a node that
    // misdescribes itself in the one field we can verify is not one to record.
    if request.id != caller.node_id.to_string() {
        return Err(Problem::new(
            crate::problem::ProblemType::IdentityMismatch,
            "the node id in the hello does not match the certificate presented",
        ));
    }

    let addr = callback_address(&caller, &request.callback_host, request.callback_port);

    Ok(Json(service.answer_hello(
        caller.node_id,
        &request,
        &caller.certificate,
        addr,
    )?))
}
