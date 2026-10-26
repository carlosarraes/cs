use crate::{state, util};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    fs,
    os::unix::fs::{symlink, DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

pub fn profile_path(alias: &str) -> Result<PathBuf> {
    let key: String = alias
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(state::state_path()?.with_file_name("profiles").join(key))
}

fn read_json(path: &Path) -> Result<Value> {
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e.into()),
    }
}

pub fn source_config() -> Result<PathBuf> {
    match std::env::var_os("CS_NATIVE_SOURCE_CONFIG") {
        Some(path) => Ok(PathBuf::from(path)),
        None => crate::claude::config_home(),
    }
}

fn auth_variable(key: &str) -> bool {
    key.starts_with("ANTHROPIC_")
        || key.starts_with("CLAUDE_CODE_OAUTH")
        || key.starts_with("CLAUDE_CODE_USE_")
        || matches!(
            key,
            "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR"
                | "CLAUDE_SECURESTORAGE_CONFIG_DIR"
                | "CLAUDE_CODE_SIMPLE"
        )
}

fn sanitize_settings(settings: &mut Value) {
    if let Some(object) = settings.as_object_mut() {
        object.remove("apiKeyHelper");
        if let Some(env) = object.get_mut("env").and_then(Value::as_object_mut) {
            env.retain(|key, _| !auth_variable(key) && key != "CLAUDE_CONFIG_DIR");
        }
    }
}

fn require_hooks(settings: &Value) -> Result<()> {
    if !settings.is_object() {
        bail!("Claude settings must be a JSON object");
    }
    if settings["disableAllHooks"] == true || settings["allowManagedHooksOnly"] == true {
        bail!("cs run requires session tracking hooks; settings disable or restrict those hooks");
    }
    Ok(())
}

fn prepare_files(
    profile: &Path,
    source: &Path,
    source_global: &Value,
    account: &state::Account,
) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(profile)?;
    fs::set_permissions(profile, fs::Permissions::from_mode(0o700))?;
    let mut config = read_json(&profile.join(".claude.json"))?;
    if let Some(email) = config["oauthAccount"]["emailAddress"].as_str() {
        if email != account.email {
            bail!(
                "saved profile identity differs from account {}; refusing to replace it",
                account.email
            );
        }
    }
    for key in [
        "projects",
        "mcpServers",
        "hasCompletedOnboarding",
        "lastOnboardingVersion",
        "installMethod",
        "userID",
        "machineID",
        "firstStartTime",
        "chromeExtension",
        "theme",
        "claudeInChromeDefaultEnabled",
        "hasCompletedClaudeInChromeOnboarding",
    ] {
        if let Some(value) = source_global.get(key) {
            config[key] = value.clone();
        }
    }
    let identity = account
        .data
        .get("oauthAccount")
        .context("saved account is missing its native login identity")?;
    if identity["emailAddress"].as_str() != Some(&account.email) {
        bail!("saved native identity does not match {}", account.email);
    }
    config["oauthAccount"] = identity.clone();
    util::atomic_write(&profile.join(".claude.json"), &serde_json::to_vec(&config)?)?;
    let mut settings = read_json(&source.join("settings.json"))?;
    require_hooks(&settings)?;
    sanitize_settings(&mut settings);
    util::atomic_write(
        &profile.join("settings.json"),
        &serde_json::to_vec(&settings)?,
    )?;
    for name in [
        "projects",
        "sessions",
        "history.jsonl",
        "tasks",
        "teams",
        "plans",
        "file-history",
        "skills",
        "agents",
        "commands",
        "plugins",
        "scripts",
        "CLAUDE.md",
        "chrome",
    ] {
        let original = source.join(name);
        let target = profile.join(name);
        if matches!(name, "projects" | "sessions") && !original.exists() {
            fs::create_dir_all(&original)?;
        }
        if original.exists() {
            if fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink())
                && fs::read_link(&target)? != original
            {
                fs::remove_file(&target)?;
            }
            if fs::symlink_metadata(&target).is_err() {
                symlink(&original, &target)?;
            }
        }
    }
    Ok(())
}

fn access_credentials(oauth: &Value) -> Value {
    let mut access = oauth.clone();
    if let Some(object) = access.as_object_mut() {
        object.remove("refreshToken");
    }
    access
}

pub fn maintain() -> Result<()> {
    let _ = crate::commands::refresh_current_if_stale();
    let accounts = state::State::load()?;
    for (alias, account) in &accounts.claude().accounts {
        if let Err(error) = crate::commands::renew_native_account(alias, &account.email) {
            eprintln!("cs: {alias}: credential renewal unavailable: {error}");
        }
    }
    sync_all()
}

pub fn maintain_account(alias: &str, email: &str) -> Result<()> {
    let profile = profile_path(alias)?;
    require_profile_identity(&profile, email)?;
    let _ = crate::commands::refresh_current_if_stale();
    crate::commands::renew_native_account(alias, email)
        .with_context(|| format!("{alias}: credential renewal unavailable"))?;
    let _credentials = crate::accounts::credential_lock()?;
    let accounts = state::State::load()?;
    let account = accounts
        .claude()
        .accounts
        .get(alias)
        .context("account removed")?;
    if account.email != email {
        bail!("{alias}: account identity changed; refusing to replace running credentials");
    }
    require_profile_identity(&profile, email)?;
    sync_credentials(&profile, &account.data["claudeAiOauth"])
}

fn require_profile_identity(profile: &Path, email: &str) -> Result<()> {
    if read_json(&profile.join(".claude.json"))?["oauthAccount"]["emailAddress"].as_str()
        != Some(email)
    {
        bail!("native profile identity changed; refusing to replace running credentials");
    }
    Ok(())
}

pub fn sync_all() -> Result<()> {
    let _credentials = crate::accounts::credential_lock()?;
    let accounts = state::State::load()?;
    for (alias, account) in &accounts.claude().accounts {
        let profile = profile_path(alias)?;
        if !profile.exists() {
            continue;
        }
        let config = read_json(&profile.join(".claude.json"))?;
        if config["oauthAccount"]["emailAddress"].as_str() != Some(&account.email) {
            continue;
        }
        sync_credentials(&profile, &account.data["claudeAiOauth"])?;
    }
    Ok(())
}

fn sync_credentials(profile: &Path, saved: &Value) -> Result<()> {
    let existing = crate::claude::read_profile_oauth(profile)?;
    if existing.as_ref().is_none_or(|oauth| {
        oauth.get("refreshToken").is_some()
            || oauth["expiresAt"].as_i64().unwrap_or(0) < saved["expiresAt"].as_i64().unwrap_or(0)
    }) {
        crate::claude::write_profile_oauth(profile, &access_credentials(saved))?;
    }
    Ok(())
}

pub fn prepare(alias: &str, account: &state::Account, source: &Path) -> Result<PathBuf> {
    let _credentials = crate::accounts::credential_lock()?;
    let profile = profile_path(alias)?;
    let source_global = if source == util::home()?.join(".claude") {
        util::home()?.join(".claude.json")
    } else {
        source.join(".claude.json")
    };
    prepare_files(&profile, source, &read_json(&source_global)?, account)?;
    sync_credentials(&profile, &account.data["claudeAiOauth"])?;
    Ok(profile)
}

pub struct NativeCommand {
    inner: Command,
    settings: PathBuf,
}
impl std::ops::Deref for NativeCommand {
    type Target = Command;
    fn deref(&self) -> &Command {
        &self.inner
    }
}
impl std::ops::DerefMut for NativeCommand {
    fn deref_mut(&mut self) -> &mut Command {
        &mut self.inner
    }
}
impl Drop for NativeCommand {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.settings);
    }
}

pub fn command(profile: &Path, mut args: Vec<OsString>) -> Result<NativeCommand> {
    for path in [
        std::env::current_dir()?.join(".claude/settings.json"),
        std::env::current_dir()?.join(".claude/settings.local.json"),
        profile.join("managed-settings.json"),
        PathBuf::from("/etc/claude-code/managed-settings.json"),
        PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json"),
    ] {
        require_hooks(&read_json(&path)?)?;
    }
    if std::env::var("CLAUDE_CODE_SAFE_MODE").is_ok_and(|v| v == "1" || v == "true") {
        bail!("cs run requires session tracking hooks; safe mode disables them");
    }
    let mut settings = json!({});
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--" {
            break;
        }
        let inline = args[i]
            .to_str()
            .and_then(|a| a.strip_prefix("--settings="))
            .map(str::to_string);
        if args[i] == "--settings" || inline.is_some() {
            let count = if inline.is_some() { 1 } else { 2 };
            let value = inline
                .or_else(|| args.get(i + 1).map(|v| v.to_string_lossy().into_owned()))
                .context("--settings needs a file or JSON object")?;
            settings = if value.trim_start().starts_with('{') {
                serde_json::from_str(&value)?
            } else {
                serde_json::from_slice(
                    &fs::read(Path::new(&value)).context("reading --settings file")?,
                )?
            };
            args.drain(i..i + count);
            continue;
        }
        if args[i].to_str().is_some_and(|a| {
            matches!(
                a.split('=').next(),
                Some(
                    "--safe-mode"
                        | "--bare"
                        | "--bg"
                        | "--background"
                        | "--cloud"
                        | "--remote"
                        | "--desktop"
                )
            )
        }) {
            bail!("cs run requires a local foreground Claude process; bare/background/cloud/desktop modes cannot preserve native session tracking");
        }
        i += 1;
    }
    require_hooks(&settings)?;
    sanitize_settings(&mut settings);
    settings["apiKeyHelper"] = json!("");
    let executable = std::env::current_exe()?;
    let executable = executable
        .to_str()
        .context("cs executable path is not UTF-8")?
        .replace('\'', "'\"'\"'");
    let hook = json!({"hooks":[{"type":"command","command":format!("'{executable}' native-event"),"timeout":5}]});
    for event in ["SessionStart", "UserPromptSubmit", "Stop"] {
        if settings.get("hooks").is_none() {
            settings["hooks"] = json!({});
        }
        if settings["hooks"].get(event).is_none() {
            settings["hooks"][event] = json!([]);
        }
        settings["hooks"][event]
            .as_array_mut()
            .context("hook settings must be arrays")?
            .push(hook.clone());
    }
    let index = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let settings_file = profile.join(format!("launch-settings-{}.json", uuid::Uuid::new_v4()));
    util::atomic_write(&settings_file, &serde_json::to_vec(&settings)?)?;
    args.splice(
        index..index,
        [
            OsString::from("--settings"),
            settings_file.clone().into_os_string(),
        ],
    );
    let mut cmd = Command::new("claude");
    cmd.args(args)
        .env("CLAUDE_CONFIG_DIR", profile)
        .env("CS_NATIVE_SOURCE_CONFIG", source_config()?);
    for key in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_PROFILE",
        "ANTHROPIC_FEDERATION_RULE_ID",
        "ANTHROPIC_ORGANIZATION_ID",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
        "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
        "CLAUDE_SECURESTORAGE_CONFIG_DIR",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
        "CLAUDE_CODE_SIMPLE",
    ] {
        cmd.env_remove(key);
    }
    Ok(NativeCommand {
        inner: cmd,
        settings: settings_file,
    })
}

pub fn verify(profile: &Path, alias: &str, email: &str) -> Result<()> {
    let output = command(profile, vec![])?
        .args(["auth", "status", "--json"])
        .output()
        .context("checking native Claude authentication")?;
    let status: Value =
        serde_json::from_slice(&output.stdout).context("Claude auth status did not return JSON")?;
    if !output.status.success()
        || status["authMethod"] != "claude.ai"
        || status["email"].as_str() != Some(email)
        || status["apiProvider"] != "firstParty"
    {
        bail!("{alias}: Claude did not confirm the expected native claude.ai login; no fallback");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renewal_refuses_missing_or_replaced_profile_identity() {
        let d = tempfile::tempdir().unwrap();
        assert!(require_profile_identity(d.path(), "original@x").is_err());
        fs::write(
            d.path().join(".claude.json"),
            json!({"oauthAccount":{"emailAddress":"replacement@x"}}).to_string(),
        )
        .unwrap();
        assert!(require_profile_identity(d.path(), "original@x").is_err());
        assert!(require_profile_identity(d.path(), "replacement@x").is_ok());
    }
    #[test]
    fn disabled_tracking_and_invalid_settings_fail_without_launching() {
        let d = tempfile::tempdir().unwrap();
        for settings in [r#"{"disableAllHooks":true}"#, "null", "[]"] {
            assert!(command(d.path(), vec!["--settings".into(), settings.into()]).is_err());
        }
        assert!(command(d.path(), vec!["--safe-mode".into()]).is_err());
        assert!(require_hooks(&json!({"allowManagedHooksOnly":true})).is_err());
    }
    fn account() -> state::Account {
        state::Account {
            email: "work@x".into(),
            data: json!({"oauthAccount":{"emailAddress":"work@x"},"claudeAiOauth":{"accessToken":"test-secret","refreshToken":"test-refresh","expiresAt":9999999999999i64}}),
        }
    }
    #[test]
    fn native_credentials_never_receive_a_refresh_grant() {
        let account = account();
        let oauth = &account.data["claudeAiOauth"];
        let access = access_credentials(oauth);
        assert!(access.get("refreshToken").is_none());
        assert_eq!(access["accessToken"], oauth["accessToken"]);
        assert!(oauth.get("refreshToken").is_some());
    }

    #[test]
    fn file_settings_preserve_customizations_without_putting_secrets_in_arguments() {
        let d = tempfile::tempdir().unwrap();
        let input = d.path().join("input.json");
        fs::write(
            &input,
            json!({"env":{"TOOL_SECRET":"secret-for-test"},"permissions":{"allow":["Read"]}})
                .to_string(),
        )
        .unwrap();
        let cmd = command(
            d.path(),
            vec![
                "--settings".into(),
                input.into_os_string(),
                "-p".into(),
                "hi".into(),
            ],
        )
        .unwrap();
        assert!(!cmd
            .get_args()
            .any(|a| a.to_string_lossy().contains("secret-for-test")));
        let settings: Value = serde_json::from_slice(&fs::read(&cmd.settings).unwrap()).unwrap();
        assert_eq!(settings["env"]["TOOL_SECRET"], "secret-for-test");
        assert_eq!(settings["permissions"]["allow"][0], "Read");
        assert_eq!(
            fs::metadata(&cmd.settings).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let path = cmd.settings.clone();
        drop(cmd);
        assert!(!path.exists());
    }

    #[test]
    fn profiles_isolate_identity_but_preserve_history_tools_and_settings() {
        let d = tempfile::tempdir().unwrap();
        let source = d.path().join("source");
        let profile = d.path().join("profile");
        fs::create_dir_all(source.join("projects")).unwrap();
        fs::write(source.join("CLAUDE.md"), "rules").unwrap();
        fs::write(source.join("settings.json"),json!({"permissions":{"defaultMode":"default"},"apiKeyHelper":"wrong-account","env":{"ANTHROPIC_API_KEY":"wrong-secret","KEEP":"yes"}}).to_string()).unwrap();
        prepare_files(&profile,&source,&json!({"oauthAccount":{"emailAddress":"personal@x"},"projects":{"project":{"hasTrustDialogAccepted":true}},"cachedArtifactRoster":["personal"]}),&account()).unwrap();
        let config: Value =
            serde_json::from_slice(&fs::read(profile.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(config["oauthAccount"]["emailAddress"], "work@x");
        assert!(config.get("cachedArtifactRoster").is_none());
        assert!(config["projects"]["project"]["hasTrustDialogAccepted"]
            .as_bool()
            .unwrap());
        assert_eq!(
            fs::canonicalize(profile.join("projects")).unwrap(),
            fs::canonicalize(source.join("projects")).unwrap()
        );
        assert_eq!(
            fs::read_to_string(profile.join("CLAUDE.md")).unwrap(),
            "rules"
        );
        let settings: Value =
            serde_json::from_slice(&fs::read(profile.join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["env"]["KEEP"], "yes");
        assert!(settings["env"].get("ANTHROPIC_API_KEY").is_none());
        assert_eq!(
            fs::metadata(&profile).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(!fs::read_to_string(profile.join(".claude.json"))
            .unwrap()
            .contains("test-secret"));
    }
    #[test]
    fn native_command_clears_alternate_auth_and_preserves_flags() {
        let temp = tempfile::tempdir().unwrap();
        let cmd = command(
            temp.path(),
            vec![
                "--chrome".into(),
                "--dangerously-skip-permissions".into(),
                "--resume".into(),
                "pm".into(),
            ],
        )
        .unwrap();
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(
            &args[..4],
            &[
                "--chrome",
                "--dangerously-skip-permissions",
                "--resume",
                "pm"
            ]
        );
        let env: std::collections::BTreeMap<_, _> = cmd.get_envs().collect();
        for key in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
        ] {
            assert_eq!(env[std::ffi::OsStr::new(key)], None);
        }
        assert_eq!(
            env[std::ffi::OsStr::new("CLAUDE_CONFIG_DIR")],
            Some(temp.path().as_os_str())
        );
    }
}
