use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;

use crate::config::Config;
use crate::function_tool::FunctionCallError;

const INDEX: &str = "MEMORY.md";
const INDEX_LINES: usize = 200;
const INDEX_BYTES: usize = 25_000;

pub(crate) async fn context(config: &Config) -> Result<String, FunctionCallError> {
    let cwd = config.cwd.to_path_buf();
    let trusted = config.active_project.is_trusted();
    tokio::task::spawn_blocking(move || {
        let directory = resolve_directory(&cwd, trusted)?;
        fs::create_dir_all(&directory).map_err(super::error)?;
        let index = read_optional(&directory.join(INDEX))?.unwrap_or_default();
        let index = index.lines().take(INDEX_LINES).collect::<Vec<_>>().join("\n");
        let end = index.floor_char_boundary(index.len().min(INDEX_BYTES));
        Ok(format!(
            "<claude_code_memory>\nUse the available shell and file-editing tools to read and update memory files, including MEMORY.md.\n\n{}\n\nCurrent MEMORY.md:\n{}\n</claude_code_memory>",
            include_str!("prompts/memory.md").replace("{memory_directory}", &directory.display().to_string()),
            &index[..end],
        ))
    })
    .await
    .map_err(super::error)?
}

fn resolve_directory(cwd: &Path, trusted: bool) -> Result<PathBuf, FunctionCallError> {
    if let Some(directory) = std::env::var_os("CLAUDE_COWORK_MEMORY_PATH_OVERRIDE") {
        return absolute_directory(&directory.to_string_lossy());
    }
    let home = dirs::home_dir().ok_or_else(|| super::error("Home directory is unavailable"))?;
    let config_override = std::env::var_os("CLAUDE_CONFIG_DIR");
    let claude_home = config_override
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));

    let mut settings = vec![managed_settings()];
    if trusted {
        for filename in ["settings.local.json", "settings.json"] {
            settings.extend(
                cwd.ancestors()
                    .map(|dir| dir.join(".claude").join(filename)),
            );
        }
    }
    settings.push(claude_home.join("settings.json"));
    for path in settings {
        if let Some(content) = read_optional(&path)? {
            #[derive(Deserialize)]
            struct Settings {
                #[serde(rename = "autoMemoryDirectory")]
                directory: Option<String>,
            }
            let settings: Settings = serde_json::from_str(&content).map_err(super::error)?;
            if let Some(directory) = settings.directory {
                return absolute_directory(&directory);
            }
        }
    }

    let project_root = codex_git_utils::repository_identity(cwd)
        .map(|identity| identity.primary_root.into_path_buf())
        .or_else(|| codex_git_utils::get_git_repo_root(cwd))
        .unwrap_or_else(|| cwd.to_path_buf());
    let project_root = dunce::canonicalize(project_root).map_err(super::error)?;
    let memory_home = std::env::var_os("CLAUDE_CODE_REMOTE_MEMORY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| claude_home.clone());
    let key = if config_override.is_some() && memory_home == claude_home {
        std::env::var("CLAUDE_CODE_PROJECT_DIR_NAME")
            .ok()
            .map(|name| project_name(&name))
            .transpose()?
    } else {
        None
    }
    .unwrap_or_else(|| project_key(&project_root.to_string_lossy()));
    Ok(memory_home.join("projects").join(key).join("memory"))
}

pub(super) fn managed_settings() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(std::env::var_os("ProgramFiles").unwrap())
            .join("ClaudeCode")
            .join("managed-settings.json")
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json")
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        PathBuf::from("/etc/claude-code/managed-settings.json")
    }
}

fn absolute_directory(value: &str) -> Result<PathBuf, FunctionCallError> {
    let path = if let Some(relative) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        dirs::home_dir()
            .ok_or_else(|| super::error("Home directory is unavailable"))?
            .join(relative)
    } else {
        PathBuf::from(value)
    };
    if !path.is_absolute() {
        return Err(super::error("Memory directory must be absolute"));
    }
    Ok(path)
}

fn project_name(value: &str) -> Result<String, FunctionCallError> {
    let lower = value.to_ascii_lowercase();
    let reserved = matches!(lower.as_str(), "con" | "prn" | "aux" | "nul")
        || (lower.len() == 4
            && (lower.starts_with("com") || lower.starts_with("lpt"))
            && lower.as_bytes()[3].is_ascii_digit());
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        || reserved
    {
        return Err(super::error("Invalid CLAUDE_CODE_PROJECT_DIR_NAME"));
    }
    Ok(value.to_string())
}

fn project_key(value: &str) -> String {
    // Claude Code's key and long-path hash operate on JavaScript UTF-16 code units.
    let mut hash = 0_i32;
    let mut key = String::new();
    for unit in value.encode_utf16() {
        hash = hash.wrapping_mul(31).wrapping_add(i32::from(unit));
        key.push(if unit <= 127 && (unit as u8).is_ascii_alphanumeric() {
            char::from(unit as u8)
        } else {
            '-'
        });
    }
    if key.len() > 200 {
        key.truncate(200);
        key.push('-');
        let mut number = hash.unsigned_abs();
        let mut digits = [b'0'; 7];
        let mut start = digits.len();
        loop {
            start -= 1;
            let digit = (number % 36) as u8;
            digits[start] = if digit < 10 {
                b'0' + digit
            } else {
                b'a' + digit - 10
            };
            number /= 36;
            if number == 0 {
                break;
            }
        }
        key.push_str(std::str::from_utf8(&digits[start..]).unwrap());
    }
    key
}

pub(super) fn read_optional(path: &Path) -> Result<Option<String>, FunctionCallError> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(super::error(error)),
    }
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;
