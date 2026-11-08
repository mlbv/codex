use std::io::IsTerminal;
use std::io::Read;
use std::sync::Arc;

use anyhow::Context;
use codex_login::AuthConfig;
use codex_login::AuthManager;
use codex_login::CLIENT_ID;
use codex_login::ServerOptions;
use codex_login::login_with_access_token;
use codex_login::login_with_api_key;
use codex_login::logout_with_revoke;
use codex_login::run_device_code_login;
use codex_login::run_login_server;
use codex_protocol::auth::AuthMode;
use codex_protocol::config_types::ForcedLoginMethod;

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Sign in to the Messages API's independent account.
    Login(LoginArgs),
    /// Sign out of the Messages API's independent account.
    Logout,
}

#[derive(Debug, clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct LoginArgs {
    /// Sign in with a device code.
    #[arg(long, conflicts_with_all = ["with_api_key", "with_access_token"])]
    device_auth: bool,

    /// Read an OpenAI API key from stdin.
    #[arg(long, conflicts_with = "with_access_token")]
    with_api_key: bool,

    /// Read a personal access token or Agent Identity token from stdin.
    #[arg(long)]
    with_access_token: bool,

    #[command(subcommand)]
    command: Option<LoginCommand>,
}

#[derive(Debug, clap::Subcommand)]
enum LoginCommand {
    /// Show the Messages API's login status.
    Status,
}

pub async fn manager(config: &AuthConfig) -> Arc<AuthManager> {
    AuthManager::from_stored_auth(
        config.clone(),
        vec![
            AuthMode::Chatgpt,
            AuthMode::ApiKey,
            AuthMode::PersonalAccessToken,
            AuthMode::AgentIdentity,
        ],
    )
    .await
}

pub async fn run_command(command: Command, config: &AuthConfig) -> anyhow::Result<()> {
    match command {
        Command::Login(args) => {
            if let Some(LoginCommand::Status) = args.command {
                let auth = manager(config)
                    .await
                    .auth_cached()
                    .context("Messages API is not logged in")?;
                eprintln!("Messages API: logged in using {}.", auth.auth_mode());
                if let Some(email) = auth.get_account_email() {
                    eprintln!("Account: {email:?}");
                }
                return Ok(());
            }

            let method = if args.with_api_key {
                ForcedLoginMethod::Api
            } else {
                ForcedLoginMethod::Chatgpt
            };
            anyhow::ensure!(
                config.is_login_method_allowed(method),
                "Messages API: {} login is disabled by the authentication policy",
                if args.with_api_key {
                    "API key"
                } else {
                    "ChatGPT"
                },
            );

            if args.with_api_key {
                let stdin = std::io::stdin();
                let api_key = read_stdin_secret(
                    stdin.lock(),
                    stdin.is_terminal(),
                    "--with-api-key",
                    "API key",
                )?;
                login_with_api_key(
                    &config.codex_home,
                    &api_key,
                    config.auth_credentials_store_mode,
                    config.keyring_backend_kind,
                )
                .context("Saving Messages API credentials")?;
            } else if args.with_access_token {
                let stdin = std::io::stdin();
                let access_token = read_stdin_secret(
                    stdin.lock(),
                    stdin.is_terminal(),
                    "--with-access-token",
                    "access token",
                )?;
                let workspaces = config.effective_chatgpt_workspaces();
                login_with_access_token(
                    &config.codex_home,
                    &access_token,
                    config.auth_credentials_store_mode,
                    workspaces.as_deref(),
                    config.chatgpt_base_url.as_deref(),
                    config.keyring_backend_kind,
                    &config.auth_route_config,
                )
                .await
                .context("Signing in to the Messages API with an access token")?;
            } else {
                let opts = ServerOptions::new(
                    config.codex_home.clone(),
                    CLIENT_ID.to_string(),
                    config.effective_chatgpt_workspaces(),
                    config.auth_credentials_store_mode,
                    config.keyring_backend_kind,
                    config.auth_route_config.clone(),
                );
                if args.device_auth {
                    run_device_code_login(opts)
                        .await
                        .context("Signing in to the Messages API with a device code")?;
                } else {
                    let server = run_login_server(opts)
                        .context("Starting the Messages API browser login")?;
                    eprintln!(
                        "Open this URL to sign in to the Messages API:\n{}",
                        server.auth_url
                    );
                    server
                        .block_until_done()
                        .await
                        .context("Signing in to the Messages API with a browser")?;
                }
            }
            eprintln!("Messages API: successfully logged in.");
        }
        Command::Logout => {
            let removed = logout_with_revoke(
                &config.codex_home,
                config.auth_credentials_store_mode,
                config.keyring_backend_kind,
                &config.auth_route_config,
            )
            .await
            .context("Signing out of the Messages API")?;
            if removed {
                eprintln!("Messages API: successfully logged out.");
            } else {
                eprintln!("Messages API is not logged in.");
            }
        }
    }
    Ok(())
}

fn read_stdin_secret(
    mut reader: impl Read,
    is_terminal: bool,
    flag: &str,
    name: &str,
) -> anyhow::Result<String> {
    anyhow::ensure!(
        !is_terminal,
        "{flag} expects the {name} piped through stdin",
    );
    let mut secret = String::new();
    reader
        .read_to_string(&mut secret)
        .with_context(|| format!("Reading the Messages API {name} from stdin"))?;
    let secret = secret.trim().to_string();
    anyhow::ensure!(!secret.is_empty(), "No {name} provided through stdin");
    Ok(secret)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::Parser;
    use codex_config::ManagedAuthPolicy;
    use codex_login::AuthCredentialsStoreMode;
    use codex_login::AuthKeyringBackendKind;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    use super::*;

    #[derive(Debug, Parser)]
    struct Cli {
        #[command(subcommand)]
        command: Command,
    }

    fn config(home: &Path) -> AuthConfig {
        AuthConfig {
            codex_home: home.to_path_buf(),
            auth_credentials_store_mode: AuthCredentialsStoreMode::File,
            keyring_backend_kind: AuthKeyringBackendKind::Direct,
            forced_login_method: None,
            chatgpt_base_url: None,
            forced_chatgpt_workspace_id: None,
            managed_auth_policy: ManagedAuthPolicy::default(),
            auth_route_config: codex_login::test_support::transport_default_auth_route_config(),
        }
    }

    fn store_api_key(config: &AuthConfig, api_key: &str) -> anyhow::Result<()> {
        login_with_api_key(
            &config.codex_home,
            api_key,
            config.auth_credentials_store_mode,
            config.keyring_backend_kind,
        )?;
        Ok(())
    }

    #[tokio::test]
    async fn account_directories_load_independent_api_keys() -> anyhow::Result<()> {
        let home = tempdir()?;
        let ordinary = config(home.path());
        let gateway = config(&home.path().join("messages-api/auth"));
        store_api_key(&ordinary, "ordinary-test-key")?;
        store_api_key(&gateway, "gateway-test-key")?;

        let ordinary_auth = manager(&ordinary)
            .await
            .auth()
            .await
            .context("ordinary auth")?;
        let gateway_auth = manager(&gateway)
            .await
            .auth()
            .await
            .context("gateway auth")?;
        assert_eq!(ordinary_auth.auth_mode(), AuthMode::ApiKey);
        assert_eq!(gateway_auth.auth_mode(), AuthMode::ApiKey);
        assert_eq!(ordinary_auth.get_token()?, "ordinary-test-key");
        assert_eq!(gateway_auth.get_token()?, "gateway-test-key");
        Ok(())
    }

    #[tokio::test]
    async fn gateway_logout_preserves_ordinary_credentials() -> anyhow::Result<()> {
        let home = tempdir()?;
        let ordinary = config(home.path());
        let gateway = config(&home.path().join("messages-api/auth"));
        store_api_key(&ordinary, "ordinary-test-key")?;
        store_api_key(&gateway, "gateway-test-key")?;
        let ordinary_file = ordinary.codex_home.join("auth.json");
        let original = std::fs::read(&ordinary_file)?;

        run_command(Command::Logout, &gateway).await?;

        assert!(manager(&gateway).await.auth().await.is_none());
        assert_eq!(std::fs::read(&ordinary_file)?, original);
        assert_eq!(
            manager(&ordinary)
                .await
                .auth()
                .await
                .context("ordinary auth after gateway logout")?
                .get_token()?,
            "ordinary-test-key"
        );
        Ok(())
    }

    #[tokio::test]
    async fn empty_gateway_does_not_use_ordinary_home_credentials() -> anyhow::Result<()> {
        let home = tempdir()?;
        let ordinary = config(home.path());
        let gateway = config(&home.path().join("messages-api/auth"));
        store_api_key(&ordinary, "ordinary-test-key")?;

        assert!(manager(&ordinary).await.auth().await.is_some());
        assert!(manager(&gateway).await.auth().await.is_none());
        let status = Cli::try_parse_from(["auth", "login", "status"])?.command;
        let error = run_command(status, &gateway).await.unwrap_err();
        assert_eq!(error.to_string(), "Messages API is not logged in");
        Ok(())
    }

    #[tokio::test]
    async fn disabled_login_methods_reject_before_reading_stdin_or_using_network()
    -> anyhow::Result<()> {
        let home = tempdir()?;
        let mut config = config(home.path());
        config.forced_login_method = Some(ForcedLoginMethod::Chatgpt);
        let command = Cli::try_parse_from(["auth", "login", "--with-api-key"])?.command;
        let error = run_command(command, &config).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "Messages API: API key login is disabled by the authentication policy"
        );

        config.forced_login_method = Some(ForcedLoginMethod::Api);
        for args in [
            vec!["auth", "login"],
            vec!["auth", "login", "--device-auth"],
            vec!["auth", "login", "--with-access-token"],
        ] {
            let command = Cli::try_parse_from(args)?.command;
            let error = run_command(command, &config).await.unwrap_err();
            assert_eq!(
                error.to_string(),
                "Messages API: ChatGPT login is disabled by the authentication policy"
            );
        }
        assert!(manager(&config).await.auth().await.is_none());
        Ok(())
    }

    #[test]
    fn parses_login_modes_status_and_logout() -> anyhow::Result<()> {
        for (args, device_auth, with_api_key, with_access_token) in [
            (vec!["auth", "login"], false, false, false),
            (vec!["auth", "login", "--device-auth"], true, false, false),
            (vec!["auth", "login", "--with-api-key"], false, true, false),
            (
                vec!["auth", "login", "--with-access-token"],
                false,
                false,
                true,
            ),
        ] {
            assert!(matches!(
                Cli::try_parse_from(args)?.command,
                Command::Login(LoginArgs {
                    device_auth: device,
                    with_api_key: api_key,
                    with_access_token: access_token,
                    command: None,
                }) if device == device_auth
                    && api_key == with_api_key
                    && access_token == with_access_token
            ));
        }
        assert!(matches!(
            Cli::try_parse_from(["auth", "login", "status"])?.command,
            Command::Login(LoginArgs {
                device_auth: false,
                with_api_key: false,
                with_access_token: false,
                command: Some(LoginCommand::Status),
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["auth", "logout"])?.command,
            Command::Logout
        ));
        Ok(())
    }

    #[test]
    fn login_flags_conflict_with_each_other_and_status() {
        let flags = ["--device-auth", "--with-api-key", "--with-access-token"];
        for flag in flags {
            for other in flags {
                if flag != other {
                    let args = ["auth", "login", flag, other];
                    assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
                }
            }
            for args in [
                ["auth", "login", flag, "status"],
                ["auth", "login", "status", flag],
            ] {
                assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
            }
        }
    }

    #[test]
    fn secrets_are_not_accepted_as_arguments() {
        for args in [
            vec!["auth", "login", "--with-api-key", "private-test-key"],
            vec!["auth", "login", "--with-api-key=private-test-key"],
            vec!["auth", "login", "--api-key", "private-test-key"],
            vec!["auth", "login", "--with-access-token", "private-test-token"],
            vec!["auth", "login", "--with-access-token=private-test-token"],
            vec!["auth", "login", "--access-token", "private-test-token"],
        ] {
            assert!(Cli::try_parse_from(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn stdin_secrets_require_nonterminal_nonempty_input() -> anyhow::Result<()> {
        for (flag, name) in [
            ("--with-api-key", "API key"),
            ("--with-access-token", "access token"),
        ] {
            assert_eq!(
                read_stdin_secret(&b"  private-test-secret\r\n"[..], false, flag, name)?,
                "private-test-secret"
            );
            for input in [&b""[..], &b" \r\n\t"[..]] {
                let error = read_stdin_secret(input, false, flag, name).unwrap_err();
                assert_eq!(
                    error.to_string(),
                    format!("No {name} provided through stdin")
                );
            }
            let error =
                read_stdin_secret(&b"private-test-secret"[..], true, flag, name).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("{flag} expects the {name} piped through stdin")
            );
            let error =
                read_stdin_secret(&b"private-test-secret\xff"[..], false, flag, name).unwrap_err();
            assert!(!format!("{error:#}").contains("private-test-secret"));
        }
        Ok(())
    }
}
