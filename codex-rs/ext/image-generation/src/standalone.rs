//! Image generation for harnesses that call the Images API themselves.

use std::sync::Arc;

use codex_api::ImageEditRequest;
use codex_api::ImageGenerationRequest;
use codex_api::ImageResponse;
use codex_core::config::Config;
use codex_login::AuthManager;
use codex_model_provider::create_model_provider;
use codex_utils_absolute_path::AbsolutePathBuf;

use crate::backend::CodexImagesBackend;
use crate::backend::ImageBackendError;
use crate::extension::ImageGenerationExtensionConfig;

/// Image generation under one config's provider.
#[derive(Clone)]
pub struct StandaloneImages {
    backend: CodexImagesBackend,
}

impl StandaloneImages {
    /// `None` when the config's provider has no Images API.
    pub fn from_config(
        config: &Config,
        auth_manager: Arc<AuthManager>,
        originator: Option<String>,
    ) -> Option<Self> {
        let config =
            ImageGenerationExtensionConfig::from_config(config, &|_| None::<AbsolutePathBuf>);
        config.available.then(|| Self {
            backend: CodexImagesBackend::new(
                create_model_provider(config.provider, Some(auth_manager)),
                config.http_client_factory,
                originator,
            ),
        })
    }

    pub async fn generate(
        &self,
        request: ImageGenerationRequest,
        turn_id: &str,
        turn_metadata: Option<&str>,
    ) -> Result<(ImageResponse, Option<String>), ImageBackendError> {
        self.backend.generate(request, turn_id, turn_metadata).await
    }

    pub async fn edit(
        &self,
        request: ImageEditRequest,
        turn_id: &str,
        turn_metadata: Option<&str>,
    ) -> Result<(ImageResponse, Option<String>), ImageBackendError> {
        self.backend.edit(request, turn_id, turn_metadata).await
    }
}
