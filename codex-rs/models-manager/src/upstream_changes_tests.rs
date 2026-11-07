use super::*;
use crate::upstream_changes::record_upstream_changes;
use codex_protocol::openai_models::ToolMessage;
use codex_protocol::openai_models::ToolMessages;
use pretty_assertions::assert_eq;

fn revised_catalog(revision: &str) -> Vec<ModelInfo> {
    let mut models = crate::bundled_models_response().unwrap().models;
    let model = models
        .iter_mut()
        .find(|model| model.slug == "gpt-6.1-sol")
        .unwrap();
    model.context_window = Some(321_000);
    let messages = model.model_messages.as_mut().unwrap();
    messages.instructions_template = Some(format!(
        "Provider introduction\n# Personality\nUpstream personality {revision}\n# Provider behavior\nUpdated backend guidance"
    ));
    messages.tools = Some(ToolMessages {
        send_user_message_async: Some(ToolMessage {
            description: Some(format!("Upstream tool description {revision}")),
            ..Default::default()
        }),
        ..Default::default()
    });
    models
}

fn reports(directory: &Path) -> Vec<PathBuf> {
    let mut paths = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

#[tokio::test]
async fn matching_bundle_has_no_changeset() {
    let home = tempdir().unwrap();
    let directory = home.path().join("history");
    let models = crate::bundled_models_response().unwrap().models;
    assert!(
        record_upstream_changes(&directory, &models)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!directory.exists());
}

#[tokio::test]
async fn distinct_revisions_survive_reordering_reverts_and_restarts() {
    let directory = tempdir().unwrap();
    let mut first = revised_catalog("A");
    let first_report = record_upstream_changes(directory.path(), &first)
        .await
        .unwrap()
        .unwrap();
    let content = std::fs::read_to_string(first_report).unwrap();
    assert!(content.contains("instructions_template"));
    assert!(content.contains("Upstream personality A"));
    assert!(content.contains("Upstream tool description A"));
    assert!(content.contains("321000"));
    assert!(content.contains("@@"));

    first.reverse();
    assert!(
        record_upstream_changes(directory.path(), &first)
            .await
            .unwrap()
            .is_none()
    );
    let second = revised_catalog("B");
    assert!(
        record_upstream_changes(directory.path(), &second)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        record_upstream_changes(directory.path(), &first)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(reports(directory.path()).len(), 2);
}

#[tokio::test]
async fn concurrent_writers_publish_one_complete_changeset() {
    let directory = tempdir().unwrap();
    let models = revised_catalog("concurrent");
    let (first, second, third) = tokio::join!(
        record_upstream_changes(directory.path(), &models),
        record_upstream_changes(directory.path(), &models),
        record_upstream_changes(directory.path(), &models),
    );
    assert_eq!(
        [first.unwrap(), second.unwrap(), third.unwrap()]
            .iter()
            .filter(|path| path.is_some())
            .count(),
        1
    );
    let paths = reports(directory.path());
    assert_eq!(paths.len(), 1);
    let content = std::fs::read_to_string(&paths[0]).unwrap();
    assert!(content.contains("Upstream personality concurrent"));
    assert!(content.contains("Upstream tool description concurrent"));
    assert!(content.ends_with('\n'));
}

#[tokio::test]
async fn ordered_model_settings_create_a_distinct_revision() {
    let directory = tempdir().unwrap();
    let mut models = vec![remote_model("ordered-model", "Ordered model", 0)];
    record_upstream_changes(directory.path(), &models)
        .await
        .unwrap()
        .unwrap();

    models[0].supported_reasoning_levels.swap(0, 1);
    assert!(
        record_upstream_changes(directory.path(), &models)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(reports(directory.path()).len(), 2);
}

#[tokio::test]
async fn newly_supplied_instructions_keep_lines_and_markdown_fences_readable() {
    let directory = tempdir().unwrap();
    let mut models = crate::bundled_models_response().unwrap().models;
    models[0]
        .model_messages
        .as_mut()
        .unwrap()
        .persistent_instructions = Some("First line\n```md\nNew guidance\n```\n".to_string());
    let path = record_upstream_changes(directory.path(), &models)
        .await
        .unwrap()
        .unwrap();
    let content = std::fs::read_to_string(path).unwrap();
    assert!(content.contains("model_messages.persistent_instructions"));
    assert!(content.contains("+First line\n"));
    assert!(content.contains("+```md\n"));
    assert!(content.contains("````diff\n"));
}

#[tokio::test]
async fn changeset_includes_models_added_and_removed_upstream() {
    let directory = tempdir().unwrap();
    let mut models = crate::bundled_models_response().unwrap().models;
    let removed = models.remove(0).slug;
    models.push(remote_model("new-upstream-model", "New upstream model", 0));
    let path = record_upstream_changes(directory.path(), &models)
        .await
        .unwrap()
        .unwrap();
    let content = std::fs::read_to_string(path).unwrap();
    assert!(content.contains(&removed));
    assert!(content.contains("new-upstream-model"));
    assert!(content.contains("New upstream model"));
}

#[tokio::test]
async fn network_and_cache_keep_backend_updates_and_share_revision_history() {
    let home = tempdir().unwrap();
    let first = revised_catalog("A");
    let second = revised_catalog("B");
    let endpoint = TestModelsEndpoint::new(vec![first.clone(), second, first]);
    let manager = openai_manager_for_tests(home.path().to_path_buf(), endpoint.clone());
    let bundled = crate::bundled_models_response().unwrap();
    let bundled_model = bundled
        .models
        .iter()
        .find(|model| model.slug == "gpt-6.1-sol")
        .unwrap();
    let bundled_template = bundled_model
        .model_messages
        .as_ref()
        .unwrap()
        .instructions_template
        .as_ref()
        .unwrap();
    let expected_personality = bundled_template
        .split_once("# Personality\n")
        .unwrap()
        .1
        .trim_start()
        .split("\n\n")
        .next()
        .unwrap();
    for revision in ["A", "B", "A"] {
        manager
            .model_catalog_with_origins(RefreshStrategy::Online, DEFAULT_HTTP_CLIENT_FACTORY)
            .await;
        let effective = manager
            .get_model_info("gpt-6.1-sol", &ModelsManagerConfig::default())
            .await;
        assert_eq!(effective.context_window, Some(321_000));
        let messages = effective.model_messages.as_ref().unwrap();
        assert_eq!(
            messages
                .tools
                .as_ref()
                .unwrap()
                .send_user_message_async
                .as_ref()
                .unwrap()
                .description,
            Some(format!("Upstream tool description {revision}"))
        );
        let instructions = messages.instructions_template.as_ref().unwrap();
        assert!(instructions.contains("Updated backend guidance"));
        assert!(instructions.contains(expected_personality));
        assert!(!instructions.contains("Upstream personality"));
    }
    let directory = home.path().join(UPSTREAM_CHANGES_DIRECTORY);
    assert_eq!(reports(&directory).len(), 2);

    let restarted = openai_manager_for_tests(home.path().to_path_buf(), endpoint.clone());
    restarted
        .model_catalog_with_origins(RefreshStrategy::Offline, DEFAULT_HTTP_CLIENT_FACTORY)
        .await;
    assert_eq!(endpoint.fetch_count(), 3);
    assert_eq!(reports(&directory).len(), 2);

    let mut entry = manager.remote_models.read().await.entry.clone();
    entry.etag = Some("a-new-etag-for-the-same-content".to_string());
    entry.fetched_at = Utc::now();
    assert!(manager.apply_remote_models(entry).await);
    assert_eq!(reports(&directory).len(), 2);
}

#[tokio::test]
async fn history_write_error_keeps_backend_catalog_usable() {
    let home = tempdir().unwrap();
    let directory = home.path().join(UPSTREAM_CHANGES_DIRECTORY);
    std::fs::write(&directory, "occupied by a file").unwrap();
    let endpoint = TestModelsEndpoint::new(vec![revised_catalog("A")]);
    let manager = openai_manager_for_tests(home.path().to_path_buf(), endpoint);
    manager
        .model_catalog_with_origins(RefreshStrategy::Online, DEFAULT_HTTP_CLIENT_FACTORY)
        .await;
    let effective = manager
        .get_model_info("gpt-6.1-sol", &ModelsManagerConfig::default())
        .await;
    assert_eq!(effective.context_window, Some(321_000));
}
