use anyhow::{Context, Result};
use dotenvy::from_path_override;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectEnvStatus {
    Ready,
    NeedsUserInput,
}

const OPTIONAL_MODEL_KEYS: [(&str, &str); 7] = [
    ("GOOSE_LEAD_PROVIDER", ""),
    ("GOOSE_LEAD_MODEL", ""),
    ("GOOSE_LEAD_TURNS", "3"),
    ("GOOSE_LEAD_FAILURE_THRESHOLD", "2"),
    ("GOOSE_LEAD_FALLBACK_TURNS", "2"),
    ("GOOSE_PLANNER_PROVIDER", ""),
    ("GOOSE_PLANNER_MODEL", ""),
];

const OPENROUTER_SECTION_KEYS: [(&str, &str); 2] = [
    ("OPENROUTER_API_KEY", ""),
    ("OPENROUTER_HOST", "https://openrouter.ai"),
];

const OPENAI_COMPAT_SECTION_KEYS: [(&str, &str); 4] = [
    ("OPENAI_API_KEY", ""),
    ("OPENAI_HOST", "https://api.openai.com"),
    ("OPENAI_BASE_PATH", "v1/chat/completions"),
    ("OPENAI_CUSTOM_HEADERS", ""),
];

fn build_initial_env_template(provider_name: Option<&str>, provider_default_model: Option<&str>) -> String {
    let default_provider = match provider_name {
        Some("openai") => "openai",
        _ => "openrouter",
    };

    let default_model = match default_provider {
        "openai" => provider_default_model
            .filter(|model| !model.trim().is_empty())
            .unwrap_or("gpt-4o-mini"),
        _ => provider_default_model
            .filter(|model| !model.trim().is_empty())
            .unwrap_or("anthropic/claude-3.5-sonnet"),
    };

    format!(
        concat!(
            "# Project-local goose configuration\n",
            "# Fill in one provider section below, then restart goose.\n\n",
            "# Active provider selection\n",
            "GOOSE_PROVIDER={}\n",
            "GOOSE_MODEL={}\n\n",
            "# OpenRouter module\n",
            "# Keep GOOSE_PROVIDER=openrouter when using this block.\n",
            "# Common model options:\n",
            "# - anthropic/claude-3.5-sonnet\n",
            "# - openai/gpt-4o\n",
            "# - google/gemini-2.5-pro\n",
            "OPENROUTER_API_KEY=\n",
            "OPENROUTER_HOST=https://openrouter.ai\n\n",
            "# OpenAI-compatible API module\n",
            "# Switch GOOSE_PROVIDER=openai when using this block.\n",
            "# Works for OpenAI, self-hosted OpenAI-compatible gateways, vLLM, one-api, New API, etc.\n",
            "# Common model options:\n",
            "# - gpt-4o-mini\n",
            "# - gpt-4.1\n",
            "# - your-internal-model\n",
            "OPENAI_API_KEY=\n",
            "OPENAI_HOST=https://api.openai.com\n",
            "OPENAI_BASE_PATH=v1/chat/completions\n",
            "OPENAI_CUSTOM_HEADERS=\n\n",
            "# Optional multi-model configuration\n",
            "# Lead/worker lets you plan with one model and execute with another.\n",
            "GOOSE_LEAD_PROVIDER=\n",
            "GOOSE_LEAD_MODEL=\n",
            "GOOSE_LEAD_TURNS=3\n",
            "GOOSE_LEAD_FAILURE_THRESHOLD=2\n",
            "GOOSE_LEAD_FALLBACK_TURNS=2\n",
            "GOOSE_PLANNER_PROVIDER=\n",
            "GOOSE_PLANNER_MODEL=\n"
        ),
        default_provider,
        default_model,
    )
}

fn build_section_appendix(
    title: &str,
    comments: &[&str],
    items: &[String],
) -> Option<String> {
    if items.is_empty() {
        return None;
    }

    let mut block = String::new();
    block.push_str(title);
    block.push('\n');
    for comment in comments {
        block.push_str(comment);
        block.push('\n');
    }
    block.push_str(&items.join("\n"));
    block.push('\n');
    Some(block)
}

fn collect_missing_entries(
    existing_entries: &BTreeMap<String, String>,
    keys: &[(&str, &str)],
) -> Vec<String> {
    let mut items = Vec::new();
    for (key, default_value) in keys {
        if !existing_entries.contains_key(*key) {
            items.push(format!("{}={}", key, default_value));
        }
    }
    items
}

fn parse_env_entries(content: &str) -> BTreeMap<String, String> {
    let mut entries = BTreeMap::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };

        let value = value.trim();
        let value = if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            &value[1..value.len() - 1]
        } else {
            value
        };

        entries.insert(key.trim().to_string(), value.to_string());
    }

    entries
}

fn seed_value(key: &str, provider_default_model: Option<&str>, allow_global_seed: bool) -> String {
    match key {
        "GOOSE_PROVIDER" => std::env::var("GOOSE_PROVIDER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                if allow_global_seed {
                    goose::config::Config::global()
                        .get_param::<String>("GOOSE_PROVIDER")
                        .ok()
                        .filter(|value| !value.trim().is_empty())
                } else {
                    None
                }
            })
            .unwrap_or_default(),
        "GOOSE_MODEL" => std::env::var("GOOSE_MODEL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| provider_default_model.map(ToOwned::to_owned))
            .or_else(|| {
                if allow_global_seed {
                    goose::config::Config::global()
                        .get_param::<String>("GOOSE_MODEL")
                        .ok()
                        .filter(|value| !value.trim().is_empty())
                } else {
                    None
                }
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

async fn ensure_project_env() -> Result<ProjectEnvStatus> {
    let cwd = std::env::current_dir().context("Failed to get current directory")?;
    let env_path = cwd.join(".env");
    let env_exists = env_path.exists();

    if env_exists {
        from_path_override(&env_path).ok();
    }

    let existing_content = if env_exists {
        fs::read_to_string(&env_path)
            .with_context(|| format!("Failed to read {}", env_path.display()))?
    } else {
        String::new()
    };
    let existing_entries = parse_env_entries(&existing_content);

    let provider_name = if env_exists {
        existing_entries
            .get("GOOSE_PROVIDER")
            .filter(|value| !value.is_empty())
            .cloned()
    } else {
        std::env::var("GOOSE_PROVIDER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                goose::config::Config::global()
                    .get_param::<String>("GOOSE_PROVIDER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
    };

    let provider_metadata = if let Some(provider_name) = provider_name.as_deref() {
        goose::providers::providers()
            .await
            .into_iter()
            .find(|(metadata, _)| metadata.name == provider_name)
            .map(|(metadata, _)| metadata)
    } else {
        None
    };

    let provider_default_model = provider_metadata
        .as_ref()
        .map(|metadata| metadata.default_model.as_str());

    let allow_global_seed = !env_exists;

    if !env_exists {
        let template = build_initial_env_template(provider_name.as_deref(), provider_default_model);
        fs::write(&env_path, template)
            .with_context(|| format!("Failed to create {}", env_path.display()))?;
    } else {
        let mut root_additions = Vec::new();
        for key in ["GOOSE_PROVIDER", "GOOSE_MODEL"] {
            match existing_entries.get(key) {
                Some(value) if !value.is_empty() => {}
                None => {
                    let value = seed_value(key, provider_default_model, allow_global_seed);
                    root_additions.push(format!("{}={}", key, value));
                }
                Some(_) => {}
            }
        }

        if let Some(metadata) = provider_metadata.as_ref() {
            for key in metadata
                .config_keys
                .iter()
                .filter(|key| (key.required || key.primary) && !key.oauth_flow)
            {
                match existing_entries.get(&key.name) {
                    Some(value) if !value.is_empty() => {}
                    None => {
                        let value = key.default.clone().unwrap_or_default();
                        root_additions.push(format!("{}={}", key.name, value));
                    }
                    Some(_) => {}
                }
            }
        }

        let openrouter_additions = collect_missing_entries(&existing_entries, &OPENROUTER_SECTION_KEYS);
        let openai_additions =
            collect_missing_entries(&existing_entries, &OPENAI_COMPAT_SECTION_KEYS);
        let optional_model_additions = collect_missing_entries(&existing_entries, &OPTIONAL_MODEL_KEYS);

        let mut sections = Vec::new();
        if !root_additions.is_empty() {
            sections.push({
                let mut block = String::from("# Added by goose: fill in any blank values below.\n");
                block.push_str(&root_additions.join("\n"));
                block.push('\n');
                block
            });
        }

        if let Some(block) = build_section_appendix(
            "# OpenRouter module",
            &[
                "# Keep GOOSE_PROVIDER=openrouter when using this block.",
                "# Common model options:",
                "# - anthropic/claude-3.5-sonnet",
                "# - openai/gpt-4o",
                "# - google/gemini-2.5-pro",
            ],
            &openrouter_additions,
        ) {
            sections.push(block);
        }

        if let Some(block) = build_section_appendix(
            "# OpenAI-compatible API module",
            &[
                "# Switch GOOSE_PROVIDER=openai when using this block.",
                "# Works for OpenAI, self-hosted OpenAI-compatible gateways, vLLM, one-api, New API, etc.",
                "# Common model options:",
                "# - gpt-4o-mini",
                "# - gpt-4.1",
                "# - your-internal-model",
            ],
            &openai_additions,
        ) {
            sections.push(block);
        }

        if let Some(block) = build_section_appendix(
            "# Optional multi-model configuration",
            &["# Lead/worker lets you plan with one model and execute with another."],
            &optional_model_additions,
        ) {
            sections.push(block);
        }

        if !sections.is_empty() {
        let mut block = String::new();
        if !existing_content.ends_with('\n') {
            block.push('\n');
        }

            block.push_str(&sections.join("\n"));

            let mut file = OpenOptions::new()
                .append(true)
                .open(&env_path)
                .with_context(|| format!("Failed to update {}", env_path.display()))?;
            file.write_all(block.as_bytes())
                .with_context(|| format!("Failed to update {}", env_path.display()))?;
        }
    }

    from_path_override(&env_path).ok();

    let final_content = fs::read_to_string(&env_path)
        .with_context(|| format!("Failed to read {}", env_path.display()))?;
    let final_entries = parse_env_entries(&final_content);

    let mut required_keys = vec!["GOOSE_PROVIDER".to_string(), "GOOSE_MODEL".to_string()];
    if let Some(provider_name) = final_entries
        .get("GOOSE_PROVIDER")
        .filter(|value| !value.is_empty())
    {
        let final_provider_metadata = if provider_metadata
            .as_ref()
            .map(|metadata| metadata.name.as_str())
            == Some(provider_name.as_str())
        {
            provider_metadata.clone()
        } else {
            goose::providers::providers()
                .await
                .into_iter()
                .find(|(metadata, _)| metadata.name == *provider_name)
                .map(|(metadata, _)| metadata)
        };

        if let Some(metadata) = final_provider_metadata {
            required_keys.extend(
                metadata
                    .config_keys
                    .into_iter()
                    .filter(|key| key.required && !key.oauth_flow)
                    .map(|key| key.name),
            );
        }
    }

    let mut missing_values = Vec::new();
    for key in required_keys {
        match final_entries.get(&key) {
            Some(value) if !value.is_empty() => {}
            _ => missing_values.push(key),
        }
    }

    if !missing_values.is_empty() {
        eprintln!(
            "Project .env is incomplete: {}\nFill in these keys and restart goose: {}",
            env_path.display(),
            missing_values.join(", ")
        );
        return Ok(ProjectEnvStatus::NeedsUserInput);
    }

    Ok(ProjectEnvStatus::Ready)
}

pub async fn prepare_project_env() -> Result<bool> {
    Ok(ensure_project_env().await? == ProjectEnvStatus::Ready)
}