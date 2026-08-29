use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_sandboxing::policy_transforms::merge_permission_profiles;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;
use tracing::warn;
use uuid::Uuid;

const PERSISTED_PERMISSION_VERSION: u32 = 1;

#[derive(Clone, Debug, Default)]
pub(crate) struct PersistentPermissionStore {
    persistence_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistentPermissionContext {
    environment_id: String,
    cwd: String,
    workspace_roots: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct PersistedPermissionGrant {
    version: u32,
    context: PersistentPermissionContext,
    permissions: AdditionalPermissionProfile,
}

impl PersistentPermissionContext {
    fn from_environment(environment: &TurnEnvironmentSelection) -> Self {
        let mut workspace_roots = environment
            .workspace_roots
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        workspace_roots.sort_unstable();
        workspace_roots.dedup();
        Self {
            environment_id: environment.environment_id.clone(),
            cwd: environment.cwd.to_string(),
            workspace_roots,
        }
    }
}

impl PersistentPermissionStore {
    /// Creates a store whose session grants are reusable by other local sessions
    /// with the exact same environment and workspace context.
    pub(crate) fn persistent(codex_home: &Path) -> Self {
        Self {
            persistence_dir: Some(codex_home.join("approvals").join("permissions")),
        }
    }

    pub(crate) fn grant(
        &self,
        environment: &TurnEnvironmentSelection,
        permissions: AdditionalPermissionProfile,
    ) {
        let context = PersistentPermissionContext::from_environment(environment);
        let grant = PersistedPermissionGrant {
            version: PERSISTED_PERMISSION_VERSION,
            context,
            permissions,
        };
        let serialized_grant = match serde_json::to_string(&grant) {
            Ok(serialized_grant) => serialized_grant,
            Err(err) => {
                warn!(error = %err, "failed to serialize persistent permission grant key");
                return;
            }
        };
        let Some(directory) = self.persistence_dir.as_ref() else {
            return;
        };
        let id = Uuid::new_v5(&Uuid::NAMESPACE_OID, serialized_grant.as_bytes());
        let path = directory.join(format!("{id}.json"));
        let contents = match serde_json::to_string_pretty(&grant) {
            Ok(contents) => contents,
            Err(err) => {
                warn!(error = %err, "failed to serialize persistent permission grant");
                return;
            }
        };
        if let Err(err) = codex_utils_path::write_atomically(&path, &contents) {
            warn!(path = %path.display(), error = %err, "failed to persist permission grant");
        }
    }

    pub(crate) fn granted(
        &self,
        environment: &TurnEnvironmentSelection,
    ) -> Option<AdditionalPermissionProfile> {
        let directory = self.persistence_dir.as_ref()?;
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
            Err(err) => {
                warn!(path = %directory.display(), error = %err, "failed to read persistent permission grants");
                return None;
            }
        };
        let expected_context = PersistentPermissionContext::from_environment(environment);
        let mut granted_permissions = None;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    warn!(path = %directory.display(), error = %err, "failed to read persistent permission grant entry");
                    continue;
                }
            };
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let contents = match std::fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(err) => {
                    warn!(path = %path.display(), error = %err, "failed to read persistent permission grant");
                    continue;
                }
            };
            let grant: PersistedPermissionGrant = match serde_json::from_str(&contents) {
                Ok(grant) => grant,
                Err(err) => {
                    warn!(path = %path.display(), error = %err, "failed to parse persistent permission grant");
                    continue;
                }
            };
            if grant.version != PERSISTED_PERMISSION_VERSION || grant.context != expected_context {
                continue;
            }
            granted_permissions =
                merge_permission_profiles(granted_permissions.as_ref(), Some(&grant.permissions));
        }
        granted_permissions
    }
}

#[cfg(test)]
#[path = "persistent_permissions_tests.rs"]
mod tests;
