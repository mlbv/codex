use super::*;
use crate::ModelsManagerConfig;
use codex_prompts::render_model_instructions;
use codex_protocol::config_types::Personality;
use codex_protocol::openai_models::ApprovalMessages;
use codex_protocol::openai_models::AppsMessages;
use codex_protocol::openai_models::AutoReviewMessages;
use codex_protocol::openai_models::CollaborationModeMessages;
use codex_protocol::openai_models::ConfirmationPolicies;
use codex_protocol::openai_models::GranularApprovalMessages;
use codex_protocol::openai_models::GuardianV2ModelConfig;
use codex_protocol::openai_models::ModelTokenBudgetConfig;
use codex_protocol::openai_models::MultiAgentMessages;
use codex_protocol::openai_models::MultiAgentModeMessages;
use codex_protocol::openai_models::MultiAgentRoleMessages;
use codex_protocol::openai_models::MultiAgentToolMessages;
use codex_protocol::openai_models::PermissionMessages;
use codex_protocol::openai_models::ToolMessage;
use codex_protocol::openai_models::ToolMessages;
use pretty_assertions::assert_eq;

fn config_with_personality(personality: Option<Personality>) -> ModelsManagerConfig {
    ModelsManagerConfig {
        personality,
        ..Default::default()
    }
}

#[test]
fn base_instruction_override_is_literal_and_preserves_catalog_messages() {
    let override_instructions = "override {{ personality }}\n# Personality\nKeep me";
    let persistent_instructions = "Follow up on the active task.";
    let async_message_description = "Catalog async message description.";
    let mut model = model_info_from_slug("unknown-model");
    let approvals = ApprovalMessages {
        on_request: Some("user approvals".to_string()),
        on_request_auto_review: Some("auto approvals".to_string()),
        never: Some("never approvals".to_string()),
        unless_trusted: Some("unless-trusted approvals".to_string()),
        granular: Some(GranularApprovalMessages {
            intro: Some("Catalog: {{ policy }}".to_string()),
            prompted_categories: Some("Allowed: {{ categories }}".to_string()),
            rejected_categories: Some("Denied: {{ categories }}".to_string()),
        }),
        ..Default::default()
    };
    let collaboration_modes = CollaborationModeMessages {
        default: Some("default instructions".to_string()),
        plan: Some("plan instructions".to_string()),
    };
    let auto_review = AutoReviewMessages {
        policy: Some("review policy".to_string()),
        policy_template: Some("review policy template".to_string()),
        node_repl_policy: None,
        rejection_instructions: Some("rejection instructions".to_string()),
        timeout_instructions: Some(String::new()),
    };
    let permissions = PermissionMessages {
        danger_full_access: Some("danger".to_string()),
        workspace_write: Some(String::new()),
        read_only: None,
        approved_prefixes: None,
        ..PermissionMessages::default()
    };
    let multi_agent = MultiAgentMessages {
        role: Some(MultiAgentRoleMessages {
            root: Some("root base".to_string()),
            subagent: Some("subagent base".to_string()),
        }),
        mode: Some(MultiAgentModeMessages {
            explicit: Some("explicit mode".to_string()),
            proactive: Some("proactive mode".to_string()),
            hint_text: Some("mode hint".to_string()),
        }),
    };
    let token_budget = ModelTokenBudgetConfig {
        enabled: false,
        use_history_notes_extension: false,
        reminder_threshold_tokens: 128,
        reminder_message_template: "budget reminder".to_string(),
        guidance_message: "budget guidance".to_string(),
        auto_compact_fallback_prompt: "compact prompt".to_string(),
        auto_compact_fallback_buffer_tokens: 64,
    };
    let guardian_v2 = GuardianV2ModelConfig {
        classifier_instructions: Some("Guardian experiment".to_string()),
        ..Default::default()
    };
    let confirmation_policies = ConfirmationPolicies {
        browser_use: Some("# Browser policy\n\n{{literal_markdown}}\n".to_string()),
        computer_use: Some("  # Native policy\r\n\n${native_markdown}\n".to_string()),
    };
    let mut messages = ModelMessages {
        apps: Some(AppsMessages {
            usage_instructions: Some("Catalog Apps instructions.".to_string()),
        }),
        content_filter_guidance: None,
        persistent_instructions: Some(persistent_instructions.to_string()),
        tools: Some(ToolMessages {
            send_user_message_async: Some(ToolMessage {
                description: Some(async_message_description.to_string()),
                ..Default::default()
            }),
            multi_agent: Some(MultiAgentToolMessages {
                spawn_agent: Some(ToolMessage {
                    description: Some("Catalog spawn description.".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        instructions_template: Some("template".to_string()),
        approvals: Some(approvals),
        collaboration_modes: Some(collaboration_modes),
        auto_review: Some(auto_review),
        permissions: Some(permissions),
        multi_agent: Some(multi_agent),
        token_budget: Some(token_budget),
        confirmation_policies: Some(confirmation_policies),
        guardian_v2: Some(guardian_v2),
    };
    model.model_messages = Some(messages.clone());
    let config = ModelsManagerConfig {
        base_instructions: Some(override_instructions.to_string()),
        personality: Some(Personality::None),
        ..Default::default()
    };

    let updated = with_config_overrides(model, &config);

    messages.instructions_template = Some(override_instructions.to_string());
    assert_eq!(updated.model_messages, Some(messages));
    assert_eq!(render_model_instructions(&updated), override_instructions);
}

#[test]
fn personality_none_strips_catalog_instruction_sources_through_the_next_h1() {
    let config = config_with_personality(Some(Personality::None));
    for (instructions, expected) in [
        (
            "Intro\n\n# Personality\n\nRemove me\n\n## Writing Style\n\nRemove me too\n\n# Safety\n\nKeep me",
            "Intro\n\n# Safety\n\nKeep me",
        ),
        ("Intro\n\n# Personality\n\nRemove me", "Intro\n\n"),
        (
            "Intro\n\n## Personality\n\nRemove me\n\n### Writing Style\nRemove this too\n\n## Planning\nKeep me",
            "Intro\n\n## Planning\nKeep me",
        ),
        (
            "Intro\n\n# Personality \n\nKeep me",
            "Intro\n\n# Personality \n\nKeep me",
        ),
        (
            "Intro\r\n\r\n# Personality\r\n\r\nRemove me\r\n\r\n## Writing Style\r\n\r\nRemove me too\r\n\r\n# General\r\n\r\nKeep me",
            "Intro\r\n\r\n# General\r\n\r\nKeep me",
        ),
    ] {
        let mut messages = ModelMessages {
            instructions_template: Some(instructions.to_string()),
            persistent_instructions: Some(String::new()),
            tools: Some(ToolMessages {
                send_user_message_async: Some(ToolMessage {
                    description: Some(String::new()),
                    ..Default::default()
                }),
                multi_agent: Some(MultiAgentToolMessages {
                    spawn_agent: Some(ToolMessage {
                        description: Some(String::new()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            approvals: Some(ApprovalMessages {
                on_request: Some("user approvals".to_string()),
                on_request_auto_review: None,
                never: None,
                unless_trusted: None,
                granular: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut model = model_info_from_slug("unknown-model");
        model.model_messages = Some(messages.clone());

        let updated = with_config_overrides(model, &config);

        messages.instructions_template = Some(expected.to_string());
        assert_eq!(updated.model_messages, Some(messages));
    }
}

#[test]
fn baked_personality_section_is_preserved_without_explicit_none() {
    let instructions = "Intro\n# Personality\nKeep me\n# General\nKeep me too";
    let configs = [
        config_with_personality(/*personality*/ None),
        config_with_personality(Some(Personality::Friendly)),
        config_with_personality(Some(Personality::Pragmatic)),
    ];

    for config in configs {
        let mut model = model_info_from_slug("unknown-model");
        model
            .model_messages
            .as_mut()
            .expect("fallback model messages")
            .instructions_template = Some(instructions.to_string());

        assert_eq!(
            render_model_instructions(&with_config_overrides(model, &config)),
            instructions
        );
    }
}

#[test]
fn explicit_empty_base_instructions_stay_empty_with_personality_none() {
    let mut model = model_info_from_slug("unknown-model");
    model
        .model_messages
        .as_mut()
        .expect("fallback model messages")
        .instructions_template = Some("Intro\n# Personality\nRemove me".to_string());
    let config = ModelsManagerConfig {
        base_instructions: Some(String::new()),
        personality: Some(Personality::None),
        ..Default::default()
    };

    let updated = with_config_overrides(model, &config);

    assert_eq!(
        updated
            .model_messages
            .as_ref()
            .and_then(|messages| messages.instructions_template.as_deref()),
        Some("")
    );
    assert_eq!(render_model_instructions(&updated), "");
}

#[test]
fn unknown_model_uses_builtin_instruction_template() {
    let model = model_info_from_slug("unknown-model");

    assert_eq!(render_model_instructions(&model), BASE_INSTRUCTIONS);
}


#[test]
fn provider_instructions_keep_their_context_around_the_bundled_personality() {
    let mut model = crate::bundled_models_response()
        .unwrap()
        .models
        .into_iter()
        .find(|model| model.slug == "gpt-6.1-sol")
        .unwrap();
    let bundled_template = render_model_instructions(&model);
    let bundled_personality = &bundled_template[bundled_template.find("# Personality\n").unwrap()
        ..bundled_template.find("# Working with the user\n").unwrap()];
    model.model_messages.as_mut().unwrap().instructions_template = Some(
        "Provider introduction\n# Personality\nProvider personality\n# Provider tools\nProvider tool instructions"
            .to_string(),
    );
    model.context_window = Some(123_456);
    model.default_verbosity = Some(codex_protocol::config_types::Verbosity::Low);

    let updated = with_config_overrides(model.clone(), &ModelsManagerConfig::default());
    let instructions = render_model_instructions(&updated);
    assert!(instructions.starts_with("Provider introduction\n"));
    assert!(instructions.contains(bundled_personality));
    assert!(!instructions.contains("Provider personality"));
    assert!(instructions.ends_with("# Provider tools\nProvider tool instructions"));
    assert_eq!(updated.context_window, model.context_window);
    assert_eq!(
        updated.default_verbosity,
        Some(codex_protocol::config_types::Verbosity::High)
    );

    let disabled = with_config_overrides(model, &config_with_personality(Some(Personality::None)));
    assert_eq!(
        render_model_instructions(&disabled),
        "Provider introduction\n# Provider tools\nProvider tool instructions"
    );
}

#[test]
fn explicit_catalog_and_instruction_file_keep_their_prompt_authority() {
    let catalog = crate::bundled_models_response().unwrap();
    let mut model = catalog
        .models
        .iter()
        .find(|model| model.slug == "gpt-6.1-sol")
        .unwrap()
        .clone();
    let explicit_template = "Explicit introduction\n# Personality\nExplicit personality";
    model.model_messages.as_mut().unwrap().instructions_template = Some(explicit_template.to_string());
    model.default_verbosity = Some(codex_protocol::config_types::Verbosity::Low);
    let config = ModelsManagerConfig {
        model_catalog: Some(catalog),
        ..Default::default()
    };
    let updated = with_config_overrides(model.clone(), &config);
    assert_eq!(render_model_instructions(&updated), explicit_template);
    assert_eq!(updated.default_verbosity, model.default_verbosity);

    let custom_instructions = "Custom instructions\n# Personality\nCustom personality";
    let config = ModelsManagerConfig {
        base_instructions: Some(custom_instructions.to_string()),
        personality: Some(Personality::None),
        ..Default::default()
    };
    let updated = with_config_overrides(model, &config);
    assert_eq!(render_model_instructions(&updated), custom_instructions);
}

#[test]
fn fallback_personality_can_be_disabled_without_removing_runtime_guidance() {
    let updated = with_config_overrides(
        model_info_from_slug("unknown-model"),
        &config_with_personality(Some(Personality::None)),
    );
    let instructions = render_model_instructions(&updated);
    let has_line = |expected: &str| instructions.lines().any(|line| line == expected);
    assert!(!has_line("## Personality"));
    assert!(has_line("# AGENTS.md spec"));
    assert!(has_line("# Tool Guidelines"));
}

#[test]
fn model_context_window_override_clamps_to_max_context_window() {
    let mut model = model_info_from_slug("unknown-model");
    model.context_window = Some(273_000);
    model.max_context_window = Some(400_000);
    let config = ModelsManagerConfig {
        model_context_window: Some(500_000),
        ..Default::default()
    };

    let updated = with_config_overrides(model.clone(), &config);
    let mut expected = model;
    expected.context_window = Some(400_000);

    assert_eq!(updated, expected);
}

#[test]
fn model_context_window_uses_model_value_without_override() {
    let mut model = model_info_from_slug("unknown-model");
    model.context_window = Some(273_000);
    model.max_context_window = Some(400_000);
    let config = ModelsManagerConfig::default();

    let updated = with_config_overrides(model.clone(), &config);

    assert_eq!(updated, model);
}
