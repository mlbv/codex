# Codex CLI

[**Codex CLI Documentation**](https://developers.openai.com/codex/cli)

## Startup environment

The user `config.toml` supports a top-level `[env]` table in both products and the standalone app-server:

```toml
[env]
CODEX_AUTHAPI_BASE_URL = "https://auth.example.test"
OTHER_VARIABLE = "literal value"
```

These string values override inherited environment variables and the user's `.env` file before clients, cached environment readers, or runtime threads initialize. Any valid variable name is accepted, including `CODEX_*`. Values are literal; an empty string sets an empty value. Restart the process after editing. Project configuration, profile tables, remote configuration, and `-c` overrides do not apply process environment settings.

The inherited `CODEX_HOME` selects the initial user file, defaulting to `~/.codex/config.toml`. An `[env]` entry changing `CODEX_HOME` affects subsequent reads and children but does not recursively load another startup environment table. Existing per-tool environment filtering still controls which variables tools inherit.
