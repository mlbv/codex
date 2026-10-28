use pretty_assertions::assert_eq;

use super::*;

#[test]
fn image_responses_preserve_generation_identifiers() {
    let response = serde_json::from_value(
        json!({"created":1,"data":[{"b64_json":"png", "generation_id":"generation-1"}]}),
    )
    .unwrap();
    assert_eq!(
        response_body(response)["data"][0]["generation_id"],
        "generation-1"
    );
}

fn request(body: Value) -> ImagesRequest {
    serde_json::from_value(body).expect("request")
}

#[test]
fn generations_default_to_auto_quality_and_size_and_ignore_openai_extras() {
    let generated = call(
        Operation::Generate,
        request(
            json!({ "prompt": "a cat", "model": "dall-e-3", "n": 2, "background": "transparent" }),
        ),
    )
    .expect("generate");
    assert_eq!(
        generated,
        Call::Generate(ImageGenerationRequest {
            prompt: "a cat".to_string(),
            background: Some(ImageBackground::Transparent),
            model: IMAGE_MODEL.to_string(),
            n: None,
            quality: Some(ImageQuality::Auto),
            size: Some("auto".to_string()),
        })
    );
}

#[test]
fn edits_take_one_to_five_inline_images_and_generations_none() {
    let image = json!({ "image_url": "data:image/png;base64,AAAA" });
    let Ok(Call::Edit(edit)) = call(
        Operation::Edit,
        request(json!({ "prompt": "hat", "images": [image], "quality": "low" })),
    ) else {
        panic!("edit");
    };
    assert_eq!(
        (edit.images, edit.quality),
        (
            vec![ImageReference::Inline {
                image_url: "data:image/png;base64,AAAA".to_string(),
            }],
            Some(ImageQuality::Low),
        )
    );
    for (operation, count) in [
        (Operation::Edit, 0),
        (Operation::Edit, 6),
        (Operation::Generate, 1),
    ] {
        let body = json!({ "prompt": "x", "images": vec![image.clone(); count] });
        assert!(call(operation, request(body)).is_err());
    }
}
