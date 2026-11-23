use meshcore_mautrix::config::Config;

#[test]
fn relative_paths_resolve_against_the_config_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        r#"
database = "data/bridge.db"
encryption_key_env = "KEY"
owner = "@owner:beeper.com"
homeserver = "https://matrix.example/_hungryserv/owner"
registration = "secrets/registration.json"
tenant = "personal"
channels = [0, 1]

[meshcore]
url = "http://127.0.0.1:8787"
token_env = "MESHCORE_TOKEN"
"#,
    )
    .unwrap();
    let config = Config::load(&path).unwrap();
    assert_eq!(config.database, dir.path().join("data/bridge.db"));
    assert_eq!(
        config.registration,
        dir.path().join("secrets/registration.json")
    );
    assert!(config.allows_channel(1));
    assert!(!config.allows_channel(2));
}

#[test]
fn rejects_duplicate_channels() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        r#"
database = "bridge.db"
encryption_key_env = "KEY"
owner = "@owner:beeper.com"
homeserver = "https://matrix.example"
registration = "registration.json"
tenant = "personal"
channels = [0, 0]

[meshcore]
url = "http://127.0.0.1:8787"
token_env = "MESHCORE_TOKEN"
"#,
    )
    .unwrap();
    assert!(Config::load(&path).is_err());
}
