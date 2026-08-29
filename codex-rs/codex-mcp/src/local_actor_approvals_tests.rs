use super::*;
use codex_rmcp_client::ElicitationResponse;
use pretty_assertions::assert_eq;
use rmcp::model::ElicitationAction;
use serde_json::json;

#[test]
fn canonicalize_json_sorts_nested_object_keys() {
    assert_eq!(
        serde_json::to_string(&canonicalize_json(&json!({
            "z": { "b": 2, "a": 1 },
            "a": [{ "d": 4, "c": 3 }],
        })))
        .expect("canonical JSON should serialize"),
        r#"{"a":[{"c":3,"d":4}],"z":{"a":1,"b":2}}"#,
    );
}

#[test]
fn persist_value_only_accepts_always_scope() {
    assert!(persist_value_supports_always(&json!("always")));
    assert!(persist_value_supports_always(&json!(["session", "always"])));
    assert!(!persist_value_supports_always(&json!("session")));
    assert!(!persist_value_supports_always(&json!(["session"])));
}

#[test]
fn accepted_approval_is_reused_by_another_local_session() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    let approval = LocalActorApproval {
        codex_home: codex_home.path().to_path_buf(),
        serialized_key: "exact-browser-operation".to_string(),
        actor_supports_always: false,
    };
    let accepted = ElicitationResponse {
        action: ElicitationAction::Accept,
        content: Some(json!({})),
        meta: None,
    };

    assert_eq!(approval.record_response(accepted.clone()), accepted);
    let next_session = LocalActorApproval {
        codex_home: codex_home.path().to_path_buf(),
        serialized_key: "exact-browser-operation".to_string(),
        actor_supports_always: false,
    };
    assert_eq!(next_session.persisted_response(), Some(accepted));
    let different_operation = LocalActorApproval {
        codex_home: codex_home.path().to_path_buf(),
        serialized_key: "different-browser-operation".to_string(),
        actor_supports_always: false,
    };
    assert_eq!(different_operation.persisted_response(), None);
}
