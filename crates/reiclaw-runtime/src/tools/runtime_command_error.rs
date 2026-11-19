use reiclaw_api::runtime_traits::DockerWorkspaceMountError;

pub(super) fn format_runtime_command_error(error: &anyhow::Error) -> String {
    if let Some(error) = error.downcast_ref::<DockerWorkspaceMountError>() {
        return match error {
            DockerWorkspaceMountError::WorkspacePath { path, source } => format!(
                "Failed to build runtime command: Failed to canonicalize Docker workspace path {path}: {source}"
            ),
            DockerWorkspaceMountError::AllowedRoot { path, source } => format!(
                "Failed to build runtime command: Failed to canonicalize Docker workspace root {path}: {source}"
            ),
        };
    }

    format!("Failed to build runtime command: {error:#}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_allowed_root_error_preserves_path_and_os_cause_through_context() {
        let error = anyhow::Error::new(DockerWorkspaceMountError::AllowedRoot {
            path: "/missing/root".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "fixture OS cause"),
        })
        .context("outer runtime context");

        let message = format_runtime_command_error(&error);

        assert!(message.contains("/missing/root"));
        assert!(message.contains("fixture OS cause"));
    }

    #[test]
    fn docker_workspace_path_error_preserves_path_and_os_cause_through_context() {
        let error = anyhow::Error::new(DockerWorkspaceMountError::WorkspacePath {
            path: "/missing/workspace".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "fixture OS cause"),
        })
        .context("outer runtime context");

        let message = format_runtime_command_error(&error);

        assert!(message.contains("/missing/workspace"));
        assert!(message.contains("fixture OS cause"));
    }

    #[test]
    fn generic_runtime_error_preserves_the_full_chain() {
        let error = anyhow::Error::msg("inner runtime cause").context("outer runtime context");

        let message = format_runtime_command_error(&error);

        assert!(message.contains("outer runtime context"));
        assert!(message.contains("inner runtime cause"));
    }
}
