use super::*;
use codex_protocol::models::NetworkPermissions;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;

fn environment(cwd: &Path, workspace_root: &Path) -> TurnEnvironmentSelection {
    TurnEnvironmentSelection {
        environment_id: "local".to_string(),
        cwd: PathUri::from_abs_path(&AbsolutePathBuf::try_from(cwd).expect("absolute cwd")),
        workspace_roots: vec![PathUri::from_abs_path(
            &AbsolutePathBuf::try_from(workspace_root).expect("absolute workspace root"),
        )],
        config: EnvironmentConfigState::Pending,
    }
}

fn network_permissions() -> AdditionalPermissionProfile {
    AdditionalPermissionProfile {
        network: Some(NetworkPermissions {
            enabled: Some(true),
        }),
        ..Default::default()
    }
}

#[test]
fn persistent_grant_is_reloaded_by_another_store() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let environment = environment(workspace.path(), workspace.path());
    let permissions = network_permissions();

    PersistentPermissionStore::persistent(codex_home.path())
        .grant(&environment, permissions.clone());

    assert_eq!(
        PersistentPermissionStore::persistent(codex_home.path()).granted(&environment),
        Some(permissions)
    );
}

#[test]
fn persistent_grant_is_bound_to_workspace_context() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    let first_workspace = tempfile::tempdir().expect("first temporary workspace");
    let second_workspace = tempfile::tempdir().expect("second temporary workspace");
    let first_environment = environment(first_workspace.path(), first_workspace.path());
    let second_environment = environment(second_workspace.path(), second_workspace.path());
    let store = PersistentPermissionStore::persistent(codex_home.path());

    store.grant(&first_environment, network_permissions());

    assert_eq!(store.granted(&second_environment), None);
}
