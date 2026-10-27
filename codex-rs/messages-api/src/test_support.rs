use std::path::Path;
use std::sync::Arc;

use codex_core::config::Config;
use codex_login::CodexAuth;
use codex_models_manager::manager::StaticModelsManager;
use codex_protocol::openai_models::ModelsResponse;

pub(crate) async fn backend(
    home: &Path,
    models: ModelsResponse,
    configure: impl FnOnce(&mut Config),
) -> crate::backend::Backend {
    let mut config =
        Config::load_default_with_cli_overrides_for_codex_home(home.to_path_buf(), Vec::new())
            .await
            .expect("config");
    configure(&mut config);
    crate::backend::Backend::new(
        Arc::new(config),
        codex_core::test_support::auth_manager_from_auth(CodexAuth::from_api_key("offline-test")),
        Arc::new(StaticModelsManager::new(None, models)),
        "installation".to_string(),
        "test".to_string(),
    )
}
