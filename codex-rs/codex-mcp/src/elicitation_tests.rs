use super::*;
use crate::McpPluginAttribution;
use crate::McpServerRegistration;
use crate::mcp::tests::test_elicitation_config;
use crate::mcp::tests::test_mcp_config;
use async_channel::Receiver;
use codex_config::BrowserUseRequirementsToml;
use codex_config::ComputerUseRequirementsToml;
use codex_config::ConfigLayerStack;
use codex_config::ConfigRequirements;
use codex_config::ConfigRequirementsToml;
use codex_config::Constrained;
use codex_protocol::mcp_approval_meta::APPROVAL_KIND_MCP_TOOL_CALL;
use codex_protocol::mcp_approval_meta::CONNECTOR_ID_KEY;
use codex_protocol::mcp_approval_meta::PERSIST_ALWAYS;
use codex_protocol::mcp_approval_meta::PERSIST_KEY;
use codex_protocol::mcp_approval_meta::PERSIST_SESSION;
use codex_protocol::mcp_approval_meta::TOOL_NAME_KEY;
use codex_protocol::mcp_approval_meta::TOOL_PARAMS_KEY;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::GranularApprovalConfig;
use pretty_assertions::assert_eq;
use rmcp::model::ElicitRequestParams;
use rmcp::model::ElicitationSchema;
use rmcp::model::RequestMetaObject;
use serde_json::Map;
use serde_json::json;
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;

type ReviewerResponse = std::result::Result<Option<ElicitationResponse>, &'static str>;

struct RecordingReviewer {
    calls: AtomicUsize,
    active_elicitations: Arc<AtomicUsize>,
    response: ReviewerResponse,
}

struct CountingReviewer {
    calls: AtomicUsize,
    response: Option<ElicitationResponse>,
}

impl CountingReviewer {
    fn new(response: Option<ElicitationResponse>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::default(),
            response,
        })
    }
}

impl ElicitationReviewer for CountingReviewer {
    fn review(
        &self,
        _request: ElicitationReviewRequest,
    ) -> BoxFuture<'static, Result<Option<ElicitationResponse>>> {
        self.calls.fetch_add(/*val*/ 1, Relaxed);
        let response = self.response.clone();
        async move { Ok(response) }.boxed()
    }
}

impl RecordingReviewer {
    fn new(response: ReviewerResponse) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::default(),
            active_elicitations: Arc::default(),
            response,
        })
    }
}

impl ElicitationReviewer for RecordingReviewer {
    fn review(
        &self,
        request: ElicitationReviewRequest,
    ) -> BoxFuture<'static, Result<Option<ElicitationResponse>>> {
        assert_eq!(request.server_name, "independent-mcp");
        self.calls.fetch_add(/*val*/ 1, Relaxed);
        let active_elicitations = self.active_elicitations.clone();
        let response = self.response.clone();
        async move {
            assert_eq!(active_elicitations.load(Relaxed), 1);
            tokio::task::yield_now().await;
            assert_eq!(active_elicitations.load(Relaxed), 1);
            response.map_err(anyhow::Error::msg)
        }
        .boxed()
    }
}

struct LifecycleRegistration(Arc<AtomicUsize>);

impl Drop for LifecycleRegistration {
    fn drop(&mut self) {
        self.0.fetch_sub(/*val*/ 1, Relaxed);
    }
}

fn approved_response() -> ElicitationResponse {
    ElicitationResponse {
        action: ElicitationAction::Accept,
        content: Some(json!({})),
        meta: Some(json!({ "approvals_reviewer": "auto_review" })),
    }
}

fn elicitation_fixture(
    approval_policy: AskForApproval,
    permission_profile: PermissionProfile,
    reviewer: Option<Arc<RecordingReviewer>>,
) -> (ElicitationRequestManager, Receiver<Event>, SendElicitation) {
    let lifecycle = reviewer.as_ref().map(|reviewer| {
        let active_elicitations = reviewer.active_elicitations.clone();
        ElicitationLifecycle::new(move || {
            active_elicitations.fetch_add(/*val*/ 1, Relaxed);
            LifecycleRegistration(active_elicitations.clone())
        })
    });
    let mut config = test_elicitation_config(
        "independent-mcp",
        approval_policy,
        permission_profile.clone(),
    );
    Arc::make_mut(&mut config)
        .server_permission_profiles
        .insert("another-independent-mcp".to_string(), permission_profile);
    let manager = ElicitationRequestManager::new(
        config,
        reviewer.map(|reviewer| reviewer as Arc<dyn ElicitationReviewer>),
        lifecycle,
        ElicitationRequestRouter::default(),
    );
    let (tx_event, events) = async_channel::bounded(1);
    let sender = manager.make_sender("independent-mcp".to_string(), Some(tx_event));
    (manager, events, sender)
}

async fn send_elicitation(sender: &SendElicitation, marker: Option<Value>) -> ElicitationResponse {
    let elicitation = Elicitation::Mcp(ElicitRequestParams::FormElicitationParams {
        meta: marker.map(|value| {
            RequestMetaObject::from(Map::from_iter([(STRICT_AUTO_REVIEW_KEY.into(), value)]))
        }),
        message: "Review this request".to_string(),
        requested_schema: ElicitationSchema::builder().build().unwrap(),
    });
    sender(RequestId::Number(7), elicitation)
        .await
        .expect("elicitation must receive a terminal response")
}

async fn assert_declined(marker: Value, response: Option<ReviewerResponse>) {
    let expected_calls = usize::from(marker == Value::Bool(true));
    let reviewer = response.map(RecordingReviewer::new);
    let (_, events, sender) = elicitation_fixture(
        AskForApproval::Never,
        PermissionProfile::Disabled,
        reviewer.clone(),
    );
    assert_eq!(
        send_elicitation(&sender, Some(marker)).await,
        strict_auto_review_decline()
    );
    if let Some(reviewer) = reviewer {
        assert_eq!(reviewer.calls.load(Relaxed), expected_calls);
    }
    assert!(events.is_empty());
}

#[test]
fn closed_event_channel_immediately_cleans_up_pending_elicitation() {
    let active_elicitations = Arc::new(AtomicUsize::new(0));
    let registrations = active_elicitations.clone();
    let lifecycle = ElicitationLifecycle::new(move || {
        registrations.fetch_add(/*val*/ 1, Relaxed);
        LifecycleRegistration(registrations.clone())
    });
    let (manager, events, sender) = elicitation_fixture(
        AskForApproval::OnRequest,
        PermissionProfile::Disabled,
        /*reviewer*/ None,
    );
    assert!(manager.update(
        test_elicitation_config(
            "independent-mcp",
            AskForApproval::OnRequest,
            PermissionProfile::Disabled
        ),
        /*reviewer*/ None,
        Some(lifecycle),
    ));
    drop(events);

    let elicitation = Elicitation::Mcp(ElicitRequestParams::FormElicitationParams {
        meta: None,
        message: "Review this request".to_string(),
        requested_schema: ElicitationSchema::builder().build().unwrap(),
    });
    let error = sender(RequestId::Number(7), elicitation)
        .now_or_never()
        .expect("closed event channel must not leave an elicitation pending")
        .expect_err("closed event channel must fail the elicitation");

    assert_eq!(
        error.to_string(),
        "failed to deliver MCP elicitation request"
    );
    assert!(
        manager
            .router
            .requests
            .lock()
            .expect("pending request router should be available")
            .is_empty()
    );
    assert_eq!(active_elicitations.load(Relaxed), 0);
}

#[tokio::test]
async fn strict_auto_review_respects_explicit_elicitation_denials() {
    for policy in [
        AskForApproval::OnRequest,
        AskForApproval::UnlessTrusted,
        AskForApproval::Never,
        AskForApproval::Granular(GranularApprovalConfig {
            sandbox_approval: true,
            rules: true,
            skill_approval: true,
            request_permissions: true,
            mcp_elicitations: false,
        }),
    ] {
        let explicitly_denied = matches!(
            policy,
            AskForApproval::Granular(config) if !config.allows_mcp_elicitations()
        );
        let reviewer = RecordingReviewer::new(Ok(Some(approved_response())));
        let (manager, events, sender) =
            elicitation_fixture(policy, PermissionProfile::Disabled, Some(reviewer.clone()));
        assert_eq!(
            send_elicitation(&sender, Some(json!(true))).await,
            if explicitly_denied {
                strict_auto_review_decline()
            } else {
                approved_response()
            }
        );
        if policy == AskForApproval::Never {
            for (server_name, marker) in [
                ("independent-mcp", Some(json!(false))),
                ("another-independent-mcp", None),
            ] {
                let sender = manager.make_sender(server_name.into(), /*tx_event*/ None);
                assert_eq!(
                    send_elicitation(&sender, marker).await,
                    ElicitationResponse {
                        meta: None,
                        ..approved_response()
                    },
                );
            }
        }
        manager.router.set_auto_deny(/*auto_deny*/ true);
        assert_eq!(
            send_elicitation(&sender, Some(json!(true))).await,
            ElicitationResponse {
                meta: None,
                ..strict_auto_review_decline()
            },
        );
        assert_eq!(
            (
                reviewer.calls.load(Relaxed),
                reviewer.active_elicitations.load(Relaxed)
            ),
            (usize::from(!explicitly_denied), 0),
        );
        assert!(events.is_empty(), "strict review must not emit an event");
    }
}

#[tokio::test]
async fn strict_auto_review_preserves_guardian_denials_and_cancellations() {
    for response in [
        ElicitationResponse {
            action: ElicitationAction::Decline,
            content: None,
            meta: Some(json!({
                "approvals_reviewer": "auto_review",
                "message": "The user has not authorized sending this data. Ask the user for approval.",
            })),
        },
        ElicitationResponse {
            action: ElicitationAction::Cancel,
            content: None,
            meta: Some(json!({ "approvals_reviewer": "auto_review" })),
        },
    ] {
        let reviewer = RecordingReviewer::new(Ok(Some(response.clone())));
        let (_, events, sender) = elicitation_fixture(
            AskForApproval::Never,
            PermissionProfile::Disabled,
            Some(reviewer.clone()),
        );
        assert_eq!(send_elicitation(&sender, Some(json!(true))).await, response);
        assert_eq!(reviewer.calls.load(Relaxed), 1);
        assert!(events.is_empty(), "strict review must not emit an event");
    }
}

#[tokio::test]
async fn strict_auto_review_fails_closed_without_a_canonical_decision() {
    for marker in ["null", "\"true\"", "1", "{}", "[true]"] {
        let marker = serde_json::from_str(marker).expect("valid malformed marker");
        assert_declined(marker, Some(Ok(Some(approved_response())))).await;
    }
    for response in [Ok(None), Err("reviewer failed")] {
        assert_declined(json!(true), Some(response)).await;
    }
    let invalid_decisions: [fn(&mut ElicitationResponse); 6] = [
        |response| {
            response.action = ElicitationAction::Decline;
            response.meta = Some(json!({ "message": "Ask the user to approve this request." }));
        },
        |response| response.action = ElicitationAction::Cancel,
        |response| response.meta = None,
        |response| response.meta = Some(json!({ "approvals_reviewer": "user" })),
        |response| response.meta = Some(json!({ "approvals_reviewer": "guardian_subagent" })),
        |response| response.content = Some(json!({ "approved_for_session": true })),
    ];
    for make_invalid in invalid_decisions {
        let mut response = approved_response();
        make_invalid(&mut response);
        assert_declined(json!(true), Some(Ok(Some(response)))).await;
    }
    assert_declined(json!(true), /*response*/ None).await;
}

#[tokio::test]
async fn reused_elicitation_senders_follow_each_servers_latest_permission_authority() {
    let mut config = crate::mcp::tests::test_mcp_config(std::env::temp_dir());
    config.approval_policy = codex_config::Constrained::allow_any(AskForApproval::Never);
    config.permission_profile = PermissionProfile::Disabled;
    config.apps_enabled = true;
    let auth = codex_login::CodexAuth::create_dummy_chatgpt_auth_for_testing();

    let hosted_server = crate::codex_apps_mcp_server_config(
        "https://example.com",
        /*apps_mcp_product_sku*/ None,
        /*originator*/ None,
    );
    let mut attached_server = hosted_server.clone();
    attached_server.environment_id = "attached".to_string();
    let mut catalog = crate::ResolvedMcpCatalog::builder();
    catalog.register(crate::McpServerRegistration::from_config(
        "attached".to_string(),
        attached_server,
    ));
    catalog.register(crate::McpServerRegistration::from_hosted_apps(
        "host",
        /*contribution_order*/ 0,
        hosted_server,
    ));
    config.mcp_server_catalog = catalog.build();
    let servers = crate::effective_mcp_servers(&config, Some(&auth));
    config.set_server_permission_profiles(
        &servers,
        [("attached".to_string(), PermissionProfile::read_only())],
    );

    let manager = ElicitationRequestManager::new(
        Arc::new(config.clone()),
        /*reviewer*/ None,
        /*lifecycle*/ None,
        ElicitationRequestRouter::default(),
    );
    let attached = manager.make_sender("attached".to_string(), /*tx_event*/ None);
    let hosted = manager.make_sender(
        crate::CODEX_APPS_MCP_SERVER_NAME.to_string(),
        /*tx_event*/ None,
    );

    assert_eq!(
        send_elicitation(&attached, /*marker*/ None).await.action,
        ElicitationAction::Decline
    );
    assert_eq!(
        send_elicitation(&hosted, /*marker*/ None).await.action,
        ElicitationAction::Accept
    );

    config.set_server_permission_profiles(
        &servers,
        [("attached".to_string(), PermissionProfile::Disabled)],
    );
    assert!(manager.update(
        Arc::new(config.clone()),
        /*reviewer*/ None,
        /*lifecycle*/ None,
    ));
    assert_eq!(
        send_elicitation(&attached, /*marker*/ None).await.action,
        ElicitationAction::Accept
    );

    let mut configured_servers = config.mcp_server_catalog.configured_servers();
    configured_servers
        .get_mut("attached")
        .expect("attached server should be registered")
        .enabled = false;
    config.mcp_server_catalog = config
        .mcp_server_catalog
        .with_materialized_servers(configured_servers);
    let servers = crate::effective_mcp_servers(&config, Some(&auth));
    config.set_server_permission_profiles(
        &servers,
        [("attached".to_string(), PermissionProfile::Disabled)],
    );
    assert!(manager.update(
        Arc::new(config.clone()),
        /*reviewer*/ None,
        /*lifecycle*/ None,
    ));
    assert_eq!(
        send_elicitation(&attached, /*marker*/ None).await.action,
        ElicitationAction::Decline
    );

    let servers = crate::effective_mcp_servers(&config, /*auth*/ None);
    config.set_server_permission_profiles(&servers, std::iter::empty());
    assert!(manager.update(
        Arc::new(config.clone()),
        /*reviewer*/ None,
        /*lifecycle*/ None,
    ));
    assert_eq!(
        send_elicitation(&hosted, /*marker*/ None).await.action,
        ElicitationAction::Decline
    );
}

async fn resolve_plugin_elicitation(
    plugin_id: &str,
    advertised_persistence: Value,
    response: ElicitationResponse,
    requirements_toml: ConfigRequirementsToml,
) -> ElicitationResponse {
    let server_name = "local-actor";
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    let config = local_actor_config(plugin_id, codex_home.path(), requirements_toml);
    let manager = ElicitationRequestManager::new(
        config,
        /*reviewer*/ None,
        /*lifecycle*/ None,
        ElicitationRequestRouter::default(),
    );
    let (tx_event, events) = async_channel::bounded(1);
    let sender = manager.make_sender(server_name.to_string(), Some(tx_event));
    let elicitation = local_actor_elicitation(advertised_persistence);
    let pending = tokio::spawn(async move {
        sender(RequestId::Number(7), elicitation)
            .await
            .expect("elicitation should resolve")
    });
    let event = events.recv().await.expect("elicitation event");
    let EventMsg::ElicitationRequest(request) = event.msg else {
        panic!("expected elicitation request event");
    };
    let routed_request_id = match request.id {
        ProtocolRequestId::String(value) => RequestId::String(value.into()),
        ProtocolRequestId::Integer(value) => RequestId::Number(value),
    };
    manager
        .router
        .resolve(server_name.to_string(), routed_request_id, response)
        .await
        .expect("elicitation response should route");
    pending.await.expect("elicitation task should finish")
}

fn local_actor_config(
    plugin_id: &str,
    codex_home: &Path,
    requirements_toml: ConfigRequirementsToml,
) -> Arc<McpConfig> {
    let server_name = "local-actor";
    let mut config = test_mcp_config(codex_home.to_path_buf());
    config.approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    config.permission_profile = PermissionProfile::Disabled;
    config
        .server_permission_profiles
        .insert(server_name.to_string(), PermissionProfile::Disabled);
    config.config_layer_stack =
        ConfigLayerStack::new(Vec::new(), ConfigRequirements::default(), requirements_toml)
            .expect("empty config stack should be valid");
    let mut catalog = crate::ResolvedMcpCatalog::builder();
    catalog.register(McpServerRegistration::from_plugin(
        server_name.to_string(),
        McpPluginAttribution::new(plugin_id.to_string(), plugin_id.to_string()),
        /*plugin_order*/ 0,
        crate::codex_apps_mcp_server_config(
            "https://example.com",
            /*apps_mcp_product_sku*/ None,
            /*originator*/ None,
        ),
    ));
    config.mcp_server_catalog = catalog.build();
    Arc::new(config)
}

fn local_actor_elicitation(advertised_persistence: Value) -> Elicitation {
    local_actor_elicitation_with_params(
        advertised_persistence,
        json!({ "origin": "https://example.com", "account": "business" }),
        /*strict_auto_review*/ false,
    )
}

fn local_actor_elicitation_with_params(
    advertised_persistence: Value,
    tool_params: Value,
    strict_auto_review: bool,
) -> Elicitation {
    let mut meta = Map::from_iter([
        (APPROVAL_KIND_KEY.into(), json!(APPROVAL_KIND_MCP_TOOL_CALL)),
        (CONNECTOR_ID_KEY.into(), json!("browser-use")),
        (TOOL_NAME_KEY.into(), json!("publish_post")),
        (TOOL_PARAMS_KEY.into(), tool_params),
        (PERSIST_KEY.into(), advertised_persistence),
    ]);
    if strict_auto_review {
        meta.insert(STRICT_AUTO_REVIEW_KEY.into(), json!(true));
    }
    Elicitation::Mcp(ElicitRequestParams::FormElicitationParams {
        meta: Some(RequestMetaObject::from(meta)),
        message: "Allow this local actor operation?".to_string(),
        requested_schema: ElicitationSchema::builder().build().unwrap(),
    })
}

async fn persist_local_actor_approval(config: Arc<McpConfig>, elicitation: Elicitation) {
    let manager = ElicitationRequestManager::new(
        config,
        /*reviewer*/ None,
        /*lifecycle*/ None,
        ElicitationRequestRouter::default(),
    );
    let (tx_event, events) = async_channel::bounded(1);
    let sender = manager.make_sender("local-actor".to_string(), Some(tx_event));
    let pending = tokio::spawn(async move {
        sender(RequestId::Number(1), elicitation)
            .await
            .expect("initial elicitation should resolve")
    });
    let event = events.recv().await.expect("initial elicitation event");
    let EventMsg::ElicitationRequest(request) = event.msg else {
        panic!("expected initial elicitation request event");
    };
    let request_id = match request.id {
        ProtocolRequestId::String(value) => RequestId::String(value.into()),
        ProtocolRequestId::Integer(value) => RequestId::Number(value),
    };
    manager
        .router
        .resolve(
            "local-actor".to_string(),
            request_id,
            ElicitationResponse {
                action: ElicitationAction::Accept,
                content: Some(json!({})),
                meta: None,
            },
        )
        .await
        .expect("initial response should route");
    assert_eq!(
        pending
            .await
            .expect("initial elicitation task should finish")
            .action,
        ElicitationAction::Accept
    );
}

#[tokio::test]
async fn bundled_local_actor_acceptance_is_promoted_to_always_persist() {
    for plugin_id in [
        "browser@openai-bundled",
        "chrome@openai-bundled",
        "computer-use@openai-bundled",
    ] {
        let response = resolve_plugin_elicitation(
            plugin_id,
            json!([PERSIST_SESSION, PERSIST_ALWAYS]),
            ElicitationResponse {
                action: ElicitationAction::Accept,
                content: Some(json!({})),
                meta: Some(json!({ PERSIST_KEY: PERSIST_SESSION })),
            },
            ConfigRequirementsToml::default(),
        )
        .await;

        assert_eq!(
            response,
            ElicitationResponse {
                action: ElicitationAction::Accept,
                content: Some(json!({})),
                meta: Some(json!({ PERSIST_KEY: PERSIST_ALWAYS })),
            },
            "plugin {plugin_id} should persist accepted approvals"
        );
    }
}

#[tokio::test]
async fn actor_level_always_promotion_requires_trusted_plugin_and_advertised_support() {
    for (plugin_id, advertised_persistence) in [
        ("browser@openai-curated-remote", json!(PERSIST_ALWAYS)),
        ("untrusted@openai-bundled", json!(PERSIST_ALWAYS)),
        ("browser@openai-bundled", json!(PERSIST_SESSION)),
    ] {
        let response = ElicitationResponse {
            action: ElicitationAction::Accept,
            content: Some(json!({})),
            meta: None,
        };
        assert_eq!(
            resolve_plugin_elicitation(
                plugin_id,
                advertised_persistence,
                response.clone(),
                ConfigRequirementsToml::default(),
            )
            .await,
            response,
        );
    }
}

#[tokio::test]
async fn managed_requirements_can_disable_local_actor_persistence() {
    for (plugin_id, requirements_toml) in [
        (
            "browser@openai-bundled",
            ConfigRequirementsToml {
                browser_use: Some(BrowserUseRequirementsToml {
                    allow_global_persistent_approval: Some(false),
                    ..BrowserUseRequirementsToml::default()
                }),
                ..ConfigRequirementsToml::default()
            },
        ),
        (
            "computer-use@openai-bundled",
            ConfigRequirementsToml {
                computer_use: Some(ComputerUseRequirementsToml {
                    allow_persistent_approval: Some(false),
                    ..ComputerUseRequirementsToml::default()
                }),
                ..ConfigRequirementsToml::default()
            },
        ),
    ] {
        let response = ElicitationResponse {
            action: ElicitationAction::Accept,
            content: Some(json!({})),
            meta: None,
        };
        assert_eq!(
            resolve_plugin_elicitation(
                plugin_id,
                json!(PERSIST_ALWAYS),
                response.clone(),
                requirements_toml,
            )
            .await,
            response,
        );
    }
}

#[tokio::test]
async fn local_actor_declines_are_never_persisted() {
    let response = ElicitationResponse {
        action: ElicitationAction::Decline,
        content: None,
        meta: None,
    };
    assert_eq!(
        resolve_plugin_elicitation(
            "browser@openai-bundled",
            json!(PERSIST_ALWAYS),
            response.clone(),
            ConfigRequirementsToml::default(),
        )
        .await,
        response,
    );
}

#[tokio::test]
async fn persistent_local_actor_approval_bypasses_reviewer() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    let config = local_actor_config(
        "browser@openai-bundled",
        codex_home.path(),
        ConfigRequirementsToml::default(),
    );
    persist_local_actor_approval(
        config.clone(),
        local_actor_elicitation(json!(PERSIST_ALWAYS)),
    )
    .await;

    let reviewer = CountingReviewer::new(/*response*/ None);
    let second_manager = ElicitationRequestManager::new(
        config,
        Some(reviewer.clone()),
        /*lifecycle*/ None,
        ElicitationRequestRouter::default(),
    );
    let (second_tx_event, second_events) = async_channel::bounded(1);
    let second_sender =
        second_manager.make_sender("local-actor".to_string(), Some(second_tx_event));
    let second_response = second_sender(
        RequestId::Number(2),
        local_actor_elicitation(json!(PERSIST_ALWAYS)),
    )
    .await
    .expect("second elicitation should use persisted approval");

    assert_eq!(second_response.action, ElicitationAction::Accept);
    assert_eq!(reviewer.calls.load(Relaxed), 0);
    assert!(second_events.is_empty());
}

#[tokio::test]
async fn strict_auto_review_does_not_use_persistent_local_actor_approval() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    let config = local_actor_config(
        "browser@openai-bundled",
        codex_home.path(),
        ConfigRequirementsToml::default(),
    );
    persist_local_actor_approval(
        config.clone(),
        local_actor_elicitation(json!(PERSIST_ALWAYS)),
    )
    .await;

    let reviewer = CountingReviewer::new(Some(approved_response()));
    let manager = ElicitationRequestManager::new(
        config,
        Some(reviewer.clone()),
        /*lifecycle*/ None,
        ElicitationRequestRouter::default(),
    );
    let (tx_event, events) = async_channel::bounded(1);
    let sender = manager.make_sender("local-actor".to_string(), Some(tx_event));
    let response = sender(
        RequestId::Number(2),
        local_actor_elicitation_with_params(
            json!(PERSIST_ALWAYS),
            json!({ "origin": "https://example.com", "account": "business" }),
            /*strict_auto_review*/ true,
        ),
    )
    .await
    .expect("strict elicitation should be reviewed");

    assert_eq!(response, approved_response());
    assert_eq!(reviewer.calls.load(Relaxed), 1);
    assert!(events.is_empty());
}

#[tokio::test]
async fn policy_denials_take_precedence_over_persistent_local_actor_approval() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    let base_config = local_actor_config(
        "browser@openai-bundled",
        codex_home.path(),
        ConfigRequirementsToml::default(),
    );
    persist_local_actor_approval(
        base_config.clone(),
        local_actor_elicitation(json!(PERSIST_ALWAYS)),
    )
    .await;

    for approval_policy in [
        AskForApproval::Never,
        AskForApproval::Granular(GranularApprovalConfig {
            sandbox_approval: true,
            rules: true,
            skill_approval: true,
            request_permissions: true,
            mcp_elicitations: false,
        }),
    ] {
        let mut config = (*base_config).clone();
        config.approval_policy = Constrained::allow_any(approval_policy);
        config.permission_profile = PermissionProfile::read_only();
        config
            .server_permission_profiles
            .insert("local-actor".to_string(), PermissionProfile::read_only());
        let manager = ElicitationRequestManager::new(
            Arc::new(config),
            /*reviewer*/ None,
            /*lifecycle*/ None,
            ElicitationRequestRouter::default(),
        );
        let (tx_event, events) = async_channel::bounded(1);
        let sender = manager.make_sender("local-actor".to_string(), Some(tx_event));
        let response = sender(
            RequestId::Number(2),
            local_actor_elicitation(json!(PERSIST_ALWAYS)),
        )
        .await
        .expect("denied elicitation should resolve");

        assert_eq!(response.action, ElicitationAction::Decline);
        assert!(events.is_empty());
    }
}

#[tokio::test]
async fn different_tool_params_do_not_reuse_persistent_local_actor_approval() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    let config = local_actor_config(
        "browser@openai-bundled",
        codex_home.path(),
        ConfigRequirementsToml::default(),
    );
    persist_local_actor_approval(
        config.clone(),
        local_actor_elicitation(json!(PERSIST_ALWAYS)),
    )
    .await;

    let manager = ElicitationRequestManager::new(
        config,
        /*reviewer*/ None,
        /*lifecycle*/ None,
        ElicitationRequestRouter::default(),
    );
    let (tx_event, events) = async_channel::bounded(1);
    let sender = manager.make_sender("local-actor".to_string(), Some(tx_event));
    let pending = tokio::spawn(async move {
        sender(
            RequestId::Number(2),
            local_actor_elicitation_with_params(
                json!(PERSIST_ALWAYS),
                json!({
                    "origin": "https://example.com",
                    "account": "business",
                    "content": "different post",
                }),
                /*strict_auto_review*/ false,
            ),
        )
        .await
        .expect("different operation should reach the user")
    });
    let event = events.recv().await.expect("different operation event");
    let EventMsg::ElicitationRequest(request) = event.msg else {
        panic!("expected different operation elicitation event");
    };
    let request_id = match request.id {
        ProtocolRequestId::String(value) => RequestId::String(value.into()),
        ProtocolRequestId::Integer(value) => RequestId::Number(value),
    };
    manager
        .router
        .resolve(
            "local-actor".to_string(),
            request_id,
            ElicitationResponse {
                action: ElicitationAction::Decline,
                content: None,
                meta: None,
            },
        )
        .await
        .expect("different operation response should route");

    assert_eq!(
        pending
            .await
            .expect("different operation task should finish")
            .action,
        ElicitationAction::Decline
    );
}
