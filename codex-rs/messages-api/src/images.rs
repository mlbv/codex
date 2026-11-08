//! OpenAI Images JSON through Codex's Images API.

use codex_api::ImageBackground;
use codex_api::ImageEditRequest;
use codex_api::ImageGenerationRequest;
use codex_api::ImageQuality;
use codex_image_generation_extension::IMAGE_MODEL;
use codex_protocol::models::ImageReference;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::backend::Backend;
use crate::error::ApiError;

const MAX_EDIT_IMAGES: usize = 5;

pub(crate) enum Operation {
    Generate,
    Edit,
}

/// Unlisted OpenAI fields (`model`, `n`, `output_format`, …) are ignored.
#[derive(Debug, Deserialize)]
pub(crate) struct ImagesRequest {
    prompt: String,
    #[serde(default)]
    images: Vec<ImageInput>,
    background: Option<ImageBackground>,
    quality: Option<ImageQuality>,
    size: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ImageInput {
    image_url: String,
}

#[derive(Debug, PartialEq)]
enum Call {
    Generate(ImageGenerationRequest),
    Edit(ImageEditRequest),
}

pub(crate) async fn run(
    backend: &Backend,
    operation: Operation,
    request: ImagesRequest,
    turn_id: &str,
    turn_metadata: Option<&str>,
) -> Result<(Value, Option<String>), ApiError> {
    let call = call(operation, request)?;
    let Some(client) = &backend.images else {
        return Err(ApiError::not_found(
            "images: this login's provider has no Images API",
        ));
    };
    let (response, request_id) = match call {
        Call::Generate(request) => client.generate(request, turn_id, turn_metadata).await,
        Call::Edit(request) => client.edit(request, turn_id, turn_metadata).await,
    }
    .map_err(|err| {
        ApiError::from_codex(err.codex_error(), 0, 0)
            .with_image_request_id(err.imagegen_request_id().map(str::to_string))
    })?;
    Ok((response_body(response), request_id))
}

fn response_body(response: codex_api::ImageResponse) -> Value {
    json!({
        "created": response.created,
        "data": response
            .data
            .into_iter()
            .map(|image| {
                let mut output = json!({ "b64_json": image.b64_json });
                if let Some(id) = image.generation_id { output["generation_id"] = json!(id); }
                output
            })
            .collect::<Vec<_>>(),
        "background": response.background,
        "quality": response.quality,
        "size": response.size,
    })
}

fn call(operation: Operation, request: ImagesRequest) -> Result<Call, ApiError> {
    let ImagesRequest {
        prompt,
        images,
        background,
        quality,
        size,
    } = request;
    let quality = quality.or(Some(ImageQuality::Auto));
    let size = size.or_else(|| Some("auto".to_string()));
    match operation {
        Operation::Generate if !images.is_empty() => Err(ApiError::invalid_request(
            "images: generations take none; use /v1/images/edits",
        )),
        Operation::Generate => Ok(Call::Generate(ImageGenerationRequest {
            prompt,
            background,
            model: IMAGE_MODEL.to_string(),
            n: None,
            quality,
            size,
        })),
        Operation::Edit if !(1..=MAX_EDIT_IMAGES).contains(&images.len()) => Err(
            ApiError::invalid_request(format!("images: 1 to {MAX_EDIT_IMAGES} required")),
        ),
        Operation::Edit => Ok(Call::Edit(ImageEditRequest {
            images: images
                .into_iter()
                .map(|image| ImageReference::Inline {
                    image_url: image.image_url,
                })
                .collect(),
            prompt,
            background,
            model: IMAGE_MODEL.to_string(),
            n: None,
            quality,
            size,
        })),
    }
}

#[cfg(test)]
#[path = "images_tests.rs"]
mod tests;
