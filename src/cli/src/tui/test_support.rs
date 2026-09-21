use super::*;

pub(super) fn test_app() -> App {
    App::new(
        bob_core::tools::jobs::JobRegistry::new(),
        Arc::new(PermissionEngine::new(Decision::Allow, None)),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        bob_core::agent::team::AgentRegistry::new(),
    )
}
