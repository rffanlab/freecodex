//! Persistent approvals for trusted local browser and computer-use actors.

use std::path::PathBuf;

use codex_protocol::mcp_approval_meta::APPROVAL_KIND_KEY;
use codex_protocol::mcp_approval_meta::APPROVAL_KIND_MCP_TOOL_CALL;
use codex_protocol::mcp_approval_meta::CONNECTOR_ID_KEY;
use codex_protocol::mcp_approval_meta::PERSIST_ALWAYS;
use codex_protocol::mcp_approval_meta::PERSIST_KEY;
use codex_protocol::mcp_approval_meta::TOOL_NAME_KEY;
use codex_protocol::mcp_approval_meta::TOOL_PARAMS_KEY;
use codex_rmcp_client::Elicitation;
use codex_rmcp_client::ElicitationResponse;
use rmcp::model::ElicitationAction;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha1::Digest;
use sha1::Sha1;
use tracing::warn;

use crate::McpConfig;
use crate::McpServerSource;

const PERSISTED_LOCAL_ACTOR_APPROVAL_VERSION: u32 = 1;

#[derive(Debug)]
pub(crate) struct LocalActorApproval {
    codex_home: PathBuf,
    serialized_key: String,
    actor_supports_always: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct PersistedLocalActorApproval {
    version: u32,
    key: String,
}

#[derive(Serialize)]
struct LocalActorApprovalKey<'a> {
    version: u32,
    plugin_id: &'a str,
    connector_id: &'a str,
    tool_name: &'a str,
    tool_params: Value,
}

impl LocalActorApproval {
    pub(crate) fn from_elicitation(
        config: &McpConfig,
        server_name: &str,
        elicitation: &Elicitation,
    ) -> Option<Self> {
        if !is_confirmation(elicitation) {
            return None;
        }
        let meta = elicitation.meta()?;
        if meta.get(APPROVAL_KIND_KEY).and_then(Value::as_str) != Some(APPROVAL_KIND_MCP_TOOL_CALL)
        {
            return None;
        }

        let server = config.mcp_server_catalog.server(server_name)?;
        let plugin_id = match server.source() {
            McpServerSource::Plugin(attribution) | McpServerSource::SelectedPlugin(attribution) => {
                attribution.plugin_id()
            }
            McpServerSource::Config
            | McpServerSource::Compatibility { .. }
            | McpServerSource::Extension { .. } => return None,
        };
        if !managed_requirements_allow_persistence(config, plugin_id) {
            return None;
        }

        let connector_id = meta.get(CONNECTOR_ID_KEY)?.as_str()?;
        let tool_name = meta.get(TOOL_NAME_KEY)?.as_str()?;
        let tool_params = canonicalize_json(meta.get(TOOL_PARAMS_KEY)?);
        let serialized_key = serde_json::to_string(&LocalActorApprovalKey {
            version: PERSISTED_LOCAL_ACTOR_APPROVAL_VERSION,
            plugin_id,
            connector_id,
            tool_name,
            tool_params,
        })
        .ok()?;

        Some(Self {
            codex_home: config.codex_home.clone(),
            serialized_key,
            actor_supports_always: meta
                .get(PERSIST_KEY)
                .is_some_and(persist_value_supports_always),
        })
    }

    pub(crate) fn persisted_response(&self) -> Option<ElicitationResponse> {
        let path = self.persistence_path();
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
            Err(err) => {
                warn!(path = %path.display(), error = %err, "failed to read persistent local actor approval");
                return None;
            }
        };
        let approval: PersistedLocalActorApproval = match serde_json::from_str(&contents) {
            Ok(approval) => approval,
            Err(err) => {
                warn!(path = %path.display(), error = %err, "failed to parse persistent local actor approval");
                return None;
            }
        };
        if approval.version != PERSISTED_LOCAL_ACTOR_APPROVAL_VERSION
            || approval.key != self.serialized_key
        {
            warn!(path = %path.display(), "ignored invalid persistent local actor approval");
            return None;
        }

        Some(self.apply_actor_persistence(ElicitationResponse {
            action: ElicitationAction::Accept,
            content: Some(serde_json::json!({})),
            meta: None,
        }))
    }

    pub(crate) fn record_response(&self, response: ElicitationResponse) -> ElicitationResponse {
        if response.action != ElicitationAction::Accept {
            return response;
        }

        let path = self.persistence_path();
        let approval = PersistedLocalActorApproval {
            version: PERSISTED_LOCAL_ACTOR_APPROVAL_VERSION,
            key: self.serialized_key.clone(),
        };
        let contents = match serde_json::to_string_pretty(&approval) {
            Ok(contents) => contents,
            Err(err) => {
                warn!(error = %err, "failed to serialize persistent local actor approval");
                return self.apply_actor_persistence(response);
            }
        };
        if let Some(parent) = path.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            warn!(path = %parent.display(), error = %err, "failed to create persistent local actor approval directory");
            return self.apply_actor_persistence(response);
        }
        if let Err(err) = codex_utils_path::write_atomically(&path, &contents) {
            warn!(path = %path.display(), error = %err, "failed to persist local actor approval");
        }

        self.apply_actor_persistence(response)
    }

    fn apply_actor_persistence(&self, mut response: ElicitationResponse) -> ElicitationResponse {
        if !self.actor_supports_always {
            return response;
        }
        let meta = response
            .meta
            .get_or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Some(meta) = meta.as_object_mut() {
            meta.insert(
                PERSIST_KEY.to_string(),
                Value::String(PERSIST_ALWAYS.to_string()),
            );
        }
        response
    }

    fn persistence_path(&self) -> PathBuf {
        let digest = format!("{:x}", Sha1::digest(self.serialized_key.as_bytes()));
        self.codex_home
            .join("approvals")
            .join("local-actors")
            .join(format!("{digest}.json"))
    }
}

fn is_confirmation(elicitation: &Elicitation) -> bool {
    match elicitation {
        Elicitation::Mcp(rmcp::model::ElicitRequestParams::FormElicitationParams {
            requested_schema,
            ..
        }) => requested_schema.properties.is_empty(),
        Elicitation::Mcp(_)
        | Elicitation::OpenAiForm { .. }
        | Elicitation::OpenAiElicitationForm { .. } => false,
    }
}

fn managed_requirements_allow_persistence(config: &McpConfig, plugin_id: &str) -> bool {
    let requirements = config.config_layer_stack.requirements_toml();
    match plugin_id {
        "browser@openai-bundled" | "chrome@openai-bundled" => {
            requirements.browser_use.as_ref().is_none_or(|browser| {
                browser.allow_global_persistent_approval != Some(false)
                    && browser
                        .default_origin_policy
                        .as_ref()
                        .is_none_or(|policy| policy.persistent_approval != Some(false))
                    && browser.origins.as_ref().is_none_or(|origins| {
                        origins
                            .values()
                            .all(|policy| policy.persistent_approval != Some(false))
                    })
            })
        }
        "computer-use@openai-bundled" => {
            requirements
                .computer_use
                .as_ref()
                .and_then(|computer| computer.allow_persistent_approval)
                != Some(false)
        }
        _ => false,
    }
}

fn persist_value_supports_always(value: &Value) -> bool {
    value.as_str() == Some(PERSIST_ALWAYS)
        || value.as_array().is_some_and(|values| {
            values
                .iter()
                .any(|value| value.as_str() == Some(PERSIST_ALWAYS))
        })
}

fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonicalize_json).collect()),
        Value::Object(values) => {
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_unstable_by_key(|(key, _)| *key);
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), canonicalize_json(value)))
                    .collect(),
            )
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.clone(),
    }
}

#[cfg(test)]
#[path = "local_actor_approvals_tests.rs"]
mod tests;
