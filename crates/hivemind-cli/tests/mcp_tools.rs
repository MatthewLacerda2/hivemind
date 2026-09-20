//! M2's integration test: every MCP tool driven through a real `rmcp` client
//! against a real daemon (SPEC §13.2, §14).
//!
//! The point of going through the client rather than calling the tool functions
//! directly is that the schema, the transport and the dispatch are part of what
//! Claude actually sees. A tool that works in Rust but does not round-trip
//! through JSON-RPC is not a working tool.
//!
//! The daemon comes from `tests/daemon`, like every other file here. This one
//! carried its own copy of it until #74, and a copy is a place a fix does not
//! reach: the retry that absorbs a port collision landed next door and never
//! came here.

use rmcp_client::ServiceExt as _;
use rmcp_client::model::CallToolRequestParams;
use rmcp_client::transport::StreamableHttpClientTransport;

mod daemon;

use daemon::Daemon;

/// The name every daemon in this file runs under. One machine, and these
/// tests are about the tools rather than about who is calling them.
const NAME: &str = "mcp";

/// Connect an MCP client to a running daemon.
async fn connect(
    daemon: &Daemon,
) -> rmcp_client::service::RunningService<rmcp_client::RoleClient, ()> {
    let transport = StreamableHttpClientTransport::from_uri(daemon.mcp_url());
    ().serve(transport).await.expect("mcp handshake succeeds")
}

/// Attach arguments to a tool call, if there are any.
fn with_args(params: CallToolRequestParams, args: &serde_json::Value) -> CallToolRequestParams {
    match args.as_object() {
        Some(object) if !object.is_empty() => params.with_arguments(object.clone()),
        _ => params,
    }
}

/// Call a tool and return its structured result as JSON.
async fn call(
    client: &rmcp_client::service::RunningService<rmcp_client::RoleClient, ()>,
    name: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    let result = client
        .call_tool(with_args(
            CallToolRequestParams::new(name.to_owned()),
            &args,
        ))
        .await
        .unwrap_or_else(|e| panic!("calling `{name}` failed: {e}"));

    assert_ne!(
        result.is_error,
        Some(true),
        "`{name}` returned an error: {:?}",
        result.content
    );

    result
        .structured_content
        .clone()
        .unwrap_or(serde_json::Value::Null)
}

#[tokio::test]
async fn the_server_advertises_exactly_the_seven_tools_the_spec_names() {
    // SPEC §9.1 says seven, and says to keep it to seven. An eighth tool is
    // usually orchestration, which hivemind deliberately does not do.
    let daemon = Daemon::start(NAME);
    let client = connect(&daemon).await;

    let tools = client.list_tools(None).await.expect("list_tools");
    let mut names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();

    assert_eq!(
        names,
        [
            "broadcast",
            "download_attachment",
            "inbox",
            "list_peers",
            "read",
            "reply",
            "send",
        ]
    );
    client.cancel().await.ok();
}

#[tokio::test]
async fn every_tool_describes_itself_for_a_model_to_read() {
    // The descriptions are the product: they are what tells Claude what `to`
    // accepts and what sender_kind means (SPEC §9.1).
    let daemon = Daemon::start(NAME);
    let client = connect(&daemon).await;

    let tools = client.list_tools(None).await.expect("list_tools");
    for tool in &tools.tools {
        let description = tool.description.as_deref().unwrap_or_default();
        assert!(
            description.len() > 40,
            "`{}` needs a description a model can act on, got {description:?}",
            tool.name
        );
    }
    client.cancel().await.ok();
}

#[tokio::test]
async fn send_then_inbox_then_read_round_trips_through_mcp() {
    let daemon = Daemon::start(NAME);
    let client = connect(&daemon).await;

    let sent = call(
        &client,
        "send",
        serde_json::json!({
            "to": ["everyone"],
            "subject": "dashboard PR",
            "body": "take a look when you get a chance",
        }),
    )
    .await;
    assert!(sent["id"].is_string());
    assert_eq!(
        sent["id"], sent["thread_id"],
        "a new message starts a thread"
    );

    let inbox = call(&client, "inbox", serde_json::json!({})).await;
    let items = inbox.as_array().expect("inbox returns an array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["subject"], "dashboard PR");
    assert_eq!(items[0]["unread"], true);

    let id = items[0]["id"].as_str().expect("id").to_owned();
    let message = call(&client, "read", serde_json::json!({ "id": id })).await;
    assert_eq!(message["body"], "take a look when you get a chance");

    // Reading marks it read, so a Claude does not see it again next turn.
    let after = call(&client, "inbox", serde_json::json!({ "unread_only": true })).await;
    assert_eq!(after.as_array().expect("array").len(), 0);

    client.cancel().await.ok();
}

#[tokio::test]
async fn anything_sent_through_mcp_is_marked_as_written_by_an_agent() {
    // SPEC §4.1: the entrypoint decides, and MCP means a Claude. A person
    // reading their inbox must be able to tell the two apart.
    let daemon = Daemon::start(NAME);
    let client = connect(&daemon).await;

    call(
        &client,
        "send",
        serde_json::json!({ "to": ["everyone"], "subject": "from a claude", "body": "x" }),
    )
    .await;

    let inbox = call(&client, "inbox", serde_json::json!({})).await;
    assert_eq!(inbox[0]["sender_kind"], "agent");
    client.cancel().await.ok();
}

#[tokio::test]
async fn a_caller_cannot_claim_to_be_human_by_passing_the_field() {
    let daemon = Daemon::start(NAME);
    let client = connect(&daemon).await;

    call(
        &client,
        "send",
        serde_json::json!({
            "to": ["everyone"],
            "subject": "pretending",
            "body": "x",
            "sender_kind": "human",
        }),
    )
    .await;

    let inbox = call(&client, "inbox", serde_json::json!({})).await;
    assert_eq!(
        inbox[0]["sender_kind"], "agent",
        "sender_kind is not the caller's to set"
    );
    client.cancel().await.ok();
}

#[tokio::test]
async fn reply_keeps_the_thread_and_broadcast_reaches_everyone() {
    let daemon = Daemon::start(NAME);
    let client = connect(&daemon).await;

    let root = call(
        &client,
        "send",
        serde_json::json!({ "to": ["everyone"], "subject": "lunch", "body": "?" }),
    )
    .await;
    let root_id = root["id"].as_str().expect("id").to_owned();
    let thread_id = root["thread_id"].as_str().expect("thread").to_owned();

    let reply = call(
        &client,
        "reply",
        serde_json::json!({ "id": root_id, "body": "1pm" }),
    )
    .await;
    assert_eq!(reply["thread_id"], thread_id);

    let broadcast = call(
        &client,
        "broadcast",
        serde_json::json!({ "subject": "standup moved", "body": "10am" }),
    )
    .await;
    assert!(broadcast["id"].is_string());
    assert_ne!(
        broadcast["thread_id"], thread_id,
        "a broadcast is its own thread"
    );

    client.cancel().await.ok();
}

#[tokio::test]
async fn list_peers_is_empty_rather_than_missing_before_anything_is_paired() {
    let daemon = Daemon::start(NAME);
    let client = connect(&daemon).await;

    let peers = call(&client, "list_peers", serde_json::json!({})).await;
    assert_eq!(peers.as_array().expect("array").len(), 0);
    client.cancel().await.ok();
}

#[tokio::test]
async fn asking_to_read_a_message_that_does_not_exist_is_the_callers_fault() {
    // Claude can recover from "bad id"; it cannot recover from "server broke".
    let daemon = Daemon::start(NAME);
    let client = connect(&daemon).await;

    let result = client
        .call_tool(with_args(
            CallToolRequestParams::new("read"),
            &serde_json::json!({ "id": "not-a-ulid" }),
        ))
        .await;

    match result {
        Err(rmcp_client::ServiceError::McpError(error)) => {
            assert_eq!(error.code, rmcp_client::model::ErrorCode::INVALID_PARAMS);
        }
        Err(other) => panic!("expected an MCP error, got {other}"),
        Ok(result) => assert_eq!(result.is_error, Some(true), "expected a failure"),
    }
    client.cancel().await.ok();
}

#[tokio::test]
async fn download_attachment_reports_a_message_with_no_such_attachment() {
    let daemon = Daemon::start(NAME);
    let client = connect(&daemon).await;

    let sent = call(
        &client,
        "send",
        serde_json::json!({ "to": ["everyone"], "subject": "no files", "body": "x" }),
    )
    .await;

    let result = client
        .call_tool(with_args(
            CallToolRequestParams::new("download_attachment"),
            &serde_json::json!({ "id": sent["id"], "sha": "0".repeat(64) }),
        ))
        .await;
    assert!(result.is_err() || result.is_ok_and(|r| r.is_error == Some(true)));

    client.cancel().await.ok();
}

#[tokio::test]
async fn the_two_resources_are_listed_and_readable() {
    let daemon = Daemon::start(NAME);
    let client = connect(&daemon).await;

    let resources = client.list_resources(None).await.expect("list_resources");
    let mut uris: Vec<&str> = resources.resources.iter().map(|r| r.uri.as_str()).collect();
    uris.sort_unstable();
    assert_eq!(uris, ["hivemind://inbox", "hivemind://peers"]);

    call(
        &client,
        "send",
        serde_json::json!({ "to": ["everyone"], "subject": "readable", "body": "x" }),
    )
    .await;

    let inbox = client
        .read_resource(rmcp_client::model::ReadResourceRequestParams::new(
            "hivemind://inbox",
        ))
        .await
        .expect("read_resource");

    let text = match &inbox.contents[0] {
        rmcp_client::model::ResourceContents::TextResourceContents { text, .. } => text.clone(),
        other => panic!("expected text, got {other:?}"),
    };
    assert!(text.contains("readable"), "got: {text}");
    assert!(
        text.contains("agent"),
        "sender_kind should be spelled out: {text}"
    );

    client.cancel().await.ok();
}

#[tokio::test]
async fn the_server_tells_claude_that_message_bodies_are_untrusted() {
    // A message body arrives from another machine. The instructions field is
    // where that warning has to live, because it is what a model reads first.
    let daemon = Daemon::start(NAME);
    let client = connect(&daemon).await;

    let info = client.peer_info().expect("server info");
    let instructions = info.instructions.clone().unwrap_or_default();
    assert!(instructions.contains("untrusted"), "got: {instructions}");
    assert!(instructions.contains("sender_kind"), "got: {instructions}");

    client.cancel().await.ok();
}

#[tokio::test]
async fn an_attachment_sent_to_ourselves_is_readable_by_path() {
    // SPEC §9.1: `read` gives back a local filesystem path, so a Claude can
    // open the file rather than being handed bytes through the protocol.
    let daemon = Daemon::start(NAME);
    let client = connect(&daemon).await;

    let files = tempfile::tempdir().expect("temp dir");
    let path = files.path().join("plan.md");
    std::fs::write(&path, b"# the plan\n\nstep one").expect("write");

    let sent = call(
        &client,
        "send",
        serde_json::json!({
            "to": ["everyone"],
            "subject": "the plan",
            "body": "attached",
            "attachments": [path.to_string_lossy()],
        }),
    )
    .await;

    let message = call(&client, "read", serde_json::json!({ "id": sent["id"] })).await;

    let attachment = &message["attachments"][0];
    assert_eq!(attachment["name"], "plan.md");
    assert_eq!(attachment["size"], 20);

    let stored = attachment["path"]
        .as_str()
        .expect("a path, since we hold it");
    assert_eq!(
        std::fs::read(stored).expect("the path should point at the file"),
        b"# the plan\n\nstep one"
    );

    client.cancel().await.ok();
}

#[tokio::test]
async fn download_attachment_returns_a_path_for_something_already_here() {
    let daemon = Daemon::start(NAME);
    let client = connect(&daemon).await;

    let files = tempfile::tempdir().expect("temp dir");
    let path = files.path().join("notes.txt");
    std::fs::write(&path, b"already local").expect("write");

    let sent = call(
        &client,
        "send",
        serde_json::json!({
            "to": ["everyone"],
            "subject": "notes",
            "body": "x",
            "attachments": [path.to_string_lossy()],
        }),
    )
    .await;

    let message = call(&client, "read", serde_json::json!({ "id": sent["id"] })).await;
    let sha = message["attachments"][0]["sha"].as_str().expect("a digest");

    let downloaded = call(
        &client,
        "download_attachment",
        serde_json::json!({ "id": sent["id"], "sha": sha }),
    )
    .await;

    let stored = downloaded["path"].as_str().expect("a path");
    assert_eq!(std::fs::read(stored).expect("read"), b"already local");

    client.cancel().await.ok();
}

#[tokio::test]
async fn an_attachment_name_that_is_a_path_is_refused() {
    // SPEC §6.3. The name is derived from the path, so this is about what a
    // caller can talk the daemon into naming a file.
    let daemon = Daemon::start(NAME);
    let client = connect(&daemon).await;

    let result = client
        .call_tool(with_args(
            CallToolRequestParams::new("send"),
            &serde_json::json!({
                "to": ["everyone"],
                "subject": "sneaky",
                "body": "x",
                "attachments": ["/etc/passwd/.."],
            }),
        ))
        .await;

    assert!(
        result.is_err() || result.is_ok_and(|r| r.is_error == Some(true)),
        "a path that does not name a file should be refused"
    );

    client.cancel().await.ok();
}
