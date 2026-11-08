use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::OnceLock;

use serde::Deserialize;

static INITIALIZED: OnceLock<Result<(), String>> = OnceLock::new();

#[derive(Deserialize)]
struct StartupConfig {
    #[serde(default)]
    env: BTreeMap<String, String>,
}

fn read_environment(home: &Path) -> io::Result<BTreeMap<String, String>> {
    let path = home.join("config.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error),
    };
    let config: StartupConfig = toml::from_str(&text).map_err(|_| {
        io::Error::other(format!("Invalid startup environment in {}", path.display()))
    })?;
    for (name, value) in &config.env {
        if name.is_empty() || name.contains(['=', '\0']) || value.contains('\0') {
            return Err(io::Error::other(format!(
                "Invalid environment entry in {}",
                path.display()
            )));
        }
    }
    Ok(config.env)
}

/// Loads `.env`, then the user config's `[env]` overrides, once per process.
/// The inherited `CODEX_HOME` selects the initial configuration file.
///
/// # Safety
/// Call before starting threads or any concurrent environment access.
pub unsafe fn initialize_environment() -> io::Result<()> {
    let result = INITIALIZED.get_or_init(|| {
        let load = || -> io::Result<()> {
            let home = codex_utils_home_dir::find_codex_home()?;
            super::load_dotenv();
            let environment = read_environment(&home)?;
            for (name, value) in environment {
                // SAFETY: the caller guarantees single-threaded startup.
                unsafe { std::env::set_var(name, value) };
            }
            Ok(())
        };
        load().map_err(|error| error.to_string())
    });
    result
        .as_ref()
        .map(|()| ())
        .map_err(|message| io::Error::other(message.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_all_names_and_keeps_empty_values_literal() -> io::Result<()> {
        let home = tempfile::tempdir()?;
        std::fs::write(
            home.path().join("config.toml"),
            "model = 'unchanged'\n[env]\nCODEX_AUTHAPI_BASE_URL = 'https://example.test'\nOTHER_PROVIDER = '$LITERAL'\nEMPTY = ''\n",
        )?;
        let values = read_environment(home.path())?;
        assert_eq!(values["CODEX_AUTHAPI_BASE_URL"], "https://example.test");
        assert_eq!(values["OTHER_PROVIDER"], "$LITERAL");
        assert_eq!(values["EMPTY"], "");
        Ok(())
    }

    #[test]
    fn rejects_invalid_entries_without_exposing_values() -> io::Result<()> {
        let home = tempfile::tempdir()?;
        for text in [
            "[env]\nTOKEN = 42",
            "[env]\nTOKEN = ['private-test-value']",
            "[env]\n'' = 'private-test-value'",
            "[env]\n'BAD=NAME' = 'private-test-value'",
            "[env]\nTOKEN = \"private-test-value\\u0000\"",
        ] {
            std::fs::write(home.path().join("config.toml"), text)?;
            let error = read_environment(home.path()).unwrap_err();
            assert!(!error.to_string().contains("private-test-value"));
        }
        Ok(())
    }

    #[test]
    fn absent_config_and_absent_table_are_empty() -> io::Result<()> {
        let home = tempfile::tempdir()?;
        assert!(read_environment(home.path())?.is_empty());
        std::fs::write(home.path().join("config.toml"), "model = 'unchanged'")?;
        assert!(read_environment(home.path())?.is_empty());
        Ok(())
    }
}
