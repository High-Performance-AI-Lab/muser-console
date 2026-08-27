//! Config parsing and key-file discipline (engine-parity rejections).

mod common;

use std::path::PathBuf;

use console_server::Config;

struct Setup {
    scratch: PathBuf,
    access_key_file: PathBuf,
    api_key_file: PathBuf,
    ui_dir: PathBuf,
}

fn setup(label: &str) -> Setup {
    let scratch = common::scratch_dir(label);
    let access_key_file = common::write_key(&scratch, "console.key", common::CONSOLE_KEY);
    let api_key_file = common::write_key(&scratch, "engine.key", common::ENGINE_KEY);
    let ui_dir = common::repo_root().join("ui");
    Setup {
        scratch,
        access_key_file,
        api_key_file,
        ui_dir,
    }
}

fn config_text(setup: &Setup, listen: &str, instances: &[(&str, &str)]) -> String {
    let mut text = format!(
        "listen = \"{listen}\"\naccess_key_file = \"{}\"\nui_dir = \"{}\"\n",
        setup.access_key_file.display(),
        setup.ui_dir.display()
    );
    for (name, base_url) in instances {
        text.push_str(&format!(
            "\n[[instance]]\nname = \"{name}\"\nbase_url = \"{base_url}\"\napi_key_file = \"{}\"\n",
            setup.api_key_file.display()
        ));
    }
    text
}

fn parse(setup: &Setup, text: &str) -> Result<Config, String> {
    Config::parse(text, &setup.scratch, None)
}

#[test]
fn happy_parse() {
    let setup = setup("config-happy");
    let text = config_text(
        &setup,
        "127.0.0.1:5959",
        &[
            ("primary", "http://127.0.0.1:8080"),
            ("second", "http://127.0.0.1:8081/"),
        ],
    );
    let config = parse(&setup, &text).expect("happy config must parse");
    assert_eq!(config.listen.to_string(), "127.0.0.1:5959");
    assert_eq!(config.instances.len(), 2);
    assert_eq!(config.instances[0].name, "primary");
    // Trailing slash is normalized away.
    assert_eq!(config.instances[1].base_url, "http://127.0.0.1:8081");
    assert_eq!(config.instances[1].authority, "127.0.0.1:8081");
    assert!(
        config.access_key == common::CONSOLE_KEY.as_bytes(),
        "access key bytes mismatch"
    );
}

#[test]
fn key_bytes_are_whitespace_trimmed() {
    let setup = setup("config-trim");
    common::write_key_bytes(&setup.scratch, "console.key", b"  \n\ttrim-me-key \r\n");
    let text = config_text(&setup, "127.0.0.1:0", &[("a", "http://127.0.0.1:1")]);
    let config = parse(&setup, &text).expect("config must parse");
    assert!(
        config.access_key == b"trim-me-key",
        "key must be trimmed of ASCII whitespace"
    );
}

#[test]
fn default_listen_and_override() {
    let setup = setup("config-listen");
    let mut text = config_text(&setup, "127.0.0.1:5959", &[("a", "http://127.0.0.1:1")]);
    text = text.replacen("listen = \"127.0.0.1:5959\"\n", "", 1);
    let config = Config::parse(&text, &setup.scratch, None).expect("default listen");
    assert_eq!(config.listen.to_string(), "127.0.0.1:5959");
    let config = Config::parse(&text, &setup.scratch, Some("127.0.0.1:0")).expect("override");
    assert_eq!(config.listen.port(), 0);
    let error = Config::parse(&text, &setup.scratch, Some("0.0.0.0:5959"))
        .err()
        .expect("non-loopback override must fail");
    assert!(error.contains("loopback"), "unexpected error: {error}");
}

#[test]
fn non_loopback_listen_rejected() {
    let setup = setup("config-lan");
    for listen in ["0.0.0.0:5959", "192.168.40.10:80", "[::]:5959"] {
        let text = config_text(&setup, listen, &[("a", "http://127.0.0.1:1")]);
        let error = parse(&setup, &text).err().expect("must reject");
        assert!(error.contains("loopback"), "unexpected error: {error}");
    }
}

fn add_console_tls(setup: &Setup, text: String, key: &std::path::Path) -> String {
    let cert = setup.scratch.join("console.crt");
    std::fs::write(&cert, b"structural certificate input").expect("write cert");
    format!(
        "tls_cert = \"{}\"\ntls_key = \"{}\"\n{text}",
        cert.display(),
        key.display()
    )
}

#[test]
fn non_loopback_listen_requires_a_complete_tls_pair() {
    let setup = setup("config-console-tls-pair");
    let key = common::write_key(&setup.scratch, "console-tls.key", "structural-key");
    let base = config_text(&setup, "0.0.0.0:5959", &[("a", "http://127.0.0.1:1")]);

    let config = parse(&setup, &add_console_tls(&setup, base.clone(), &key))
        .expect("a complete pair permits a non-loopback bind");
    assert!(config.tls.is_some());

    let cert = setup.scratch.join("only.crt");
    std::fs::write(&cert, b"structural certificate input").expect("write cert");
    for prefix in [
        format!("tls_cert = \"{}\"\n", cert.display()),
        format!("tls_key = \"{}\"\n", key.display()),
    ] {
        let error = parse(&setup, &format!("{prefix}{base}"))
            .err()
            .expect("a half pair must fail");
        assert!(
            error.contains("configured together"),
            "unexpected error: {error}"
        );
    }
}

#[test]
fn console_tls_key_requires_exact_0600_and_no_symlink() {
    use std::os::unix::fs::PermissionsExt as _;

    let setup = setup("config-console-tls-key");
    let base = config_text(&setup, "0.0.0.0:5959", &[("a", "http://127.0.0.1:1")]);
    for mode in [0o400, 0o640, 0o4600] {
        let key = setup.scratch.join(format!("mode-{mode:o}.key"));
        std::fs::write(&key, b"structural-key").expect("write key");
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(mode)).expect("chmod");
        let error = parse(&setup, &add_console_tls(&setup, base.clone(), &key))
            .err()
            .expect("non-exact mode must fail");
        assert!(error.contains("exact mode 0600"), "mode {mode:o}: {error}");
    }

    let target = common::write_key(&setup.scratch, "target.key", "structural-key");
    let link = setup.scratch.join("tls-link.key");
    std::os::unix::fs::symlink(target, &link).expect("symlink");
    let error = parse(&setup, &add_console_tls(&setup, base, &link))
        .err()
        .expect("symlink key must fail");
    assert!(error.contains("regular file"), "unexpected error: {error}");
}

#[test]
fn https_base_urls_are_accepted_with_isolated_platform_trust() {
    let setup = setup("config-https");
    let text = config_text(
        &setup,
        "127.0.0.1:0",
        &[
            ("a", "https://engine-a.test:8443"),
            ("b", "https://engine-b.test:8443/"),
        ],
    );
    let config = parse(&setup, &text).expect("https engines must parse");
    assert_eq!(config.instances[0].base_url, "https://engine-a.test:8443");
    assert!(config.instances[0].is_https);
    assert!(config.instances[1].is_https);
    assert!(
        !std::sync::Arc::ptr_eq(
            &config.instances[0].tls_config,
            &config.instances[1].tls_config
        ),
        "every engine must own a separate trust configuration"
    );
}

#[test]
fn plaintext_engines_require_a_literal_loopback_ip() {
    let setup = setup("config-plaintext-engine");
    for base_url in [
        "http://localhost:8080",
        "http://192.0.2.10:8080",
        "http://0.0.0.0:8080",
        "http://[2001:db8::10]:8080",
    ] {
        let text = config_text(&setup, "127.0.0.1:0", &[("a", base_url)]);
        let error = parse(&setup, &text).err().expect("must reject plaintext");
        assert!(
            error.contains("literal loopback") && error.contains("require HTTPS"),
            "for {base_url}: unexpected error: {error}"
        );
    }
    for base_url in ["http://127.0.0.1:8080", "http://[::1]:8080"] {
        parse(
            &setup,
            &config_text(&setup, "127.0.0.1:0", &[("a", base_url)]),
        )
        .unwrap_or_else(|error| panic!("{base_url} must be accepted: {error}"));
    }
}

#[test]
fn engine_ca_files_are_https_only_and_must_contain_certificates() {
    let setup = setup("config-engine-ca");
    let ca = setup.scratch.join("engine-ca.pem");
    std::fs::write(&ca, b"not a certificate").expect("write structural CA input");

    let add_ca = |base_url: &str| {
        config_text(&setup, "127.0.0.1:0", &[("a", base_url)]).replace(
            &format!("api_key_file = \"{}\"\n", setup.api_key_file.display()),
            &format!(
                "api_key_file = \"{}\"\nca_file = \"{}\"\n",
                setup.api_key_file.display(),
                ca.display()
            ),
        )
    };
    let error = parse(&setup, &add_ca("http://127.0.0.1:1"))
        .err()
        .expect("HTTP with ca_file must fail");
    assert!(error.contains("only valid with an https://"), "{error}");

    let error = parse(&setup, &add_ca("https://engine.test:8443"))
        .err()
        .expect("a CA file with no PEM certificates must fail");
    assert!(error.contains("contains no certificates"), "{error}");
}

#[test]
fn malformed_base_urls_rejected() {
    let setup = setup("config-url");
    for (base_url, needle) in [
        ("http://user@127.0.0.1:1", "userinfo"),
        ("http://127.0.0.1:1?x=1", "query"),
        ("http://127.0.0.1:1#frag", "fragment"),
        ("http://127.0.0.1:1/api", "no path"),
        ("ftp://127.0.0.1:1", "http://"),
        ("http://", "no path"),
    ] {
        let text = config_text(&setup, "127.0.0.1:0", &[("a", base_url)]);
        let error = parse(&setup, &text).err().expect("must reject");
        assert!(
            error.contains(needle),
            "for {base_url}: unexpected error: {error}"
        );
    }
}

#[test]
fn missing_instance_rejected() {
    let setup = setup("config-noinst");
    let text = config_text(&setup, "127.0.0.1:0", &[]);
    let error = parse(&setup, &text).err().expect("must reject");
    assert!(error.contains("at least one"), "unexpected error: {error}");
}

#[test]
fn instance_names_validated() {
    let setup = setup("config-names");
    let long = "x".repeat(65);
    for name in ["bad name", "bad!name", "", long.as_str()] {
        let text = config_text(&setup, "127.0.0.1:0", &[(name, "http://127.0.0.1:1")]);
        let error = parse(&setup, &text).err().expect("must reject");
        assert!(error.contains("[A-Za-z0-9_-]"), "unexpected error: {error}");
    }
    let text = config_text(
        &setup,
        "127.0.0.1:0",
        &[
            ("twin", "http://127.0.0.1:1"),
            ("twin", "http://127.0.0.1:2"),
        ],
    );
    let error = parse(&setup, &text).err().expect("must reject duplicate");
    assert!(error.contains("duplicate"), "unexpected error: {error}");
}

#[test]
fn group_writable_key_rejected() {
    use std::os::unix::fs::PermissionsExt as _;
    let setup = setup("config-mode");
    let path = setup.scratch.join("loose.key");
    std::fs::write(&path, "irrelevant-value").expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let text = config_text(&setup, "127.0.0.1:0", &[("a", "http://127.0.0.1:1")]).replace(
        &setup.access_key_file.display().to_string(),
        &path.display().to_string(),
    );
    let error = parse(&setup, &text).err().expect("must reject 0644");
    assert!(error.contains("0600"), "unexpected error: {error}");
}

#[test]
fn symlink_key_rejected() {
    let setup = setup("config-symlink");
    let link = setup.scratch.join("link.key");
    std::os::unix::fs::symlink(&setup.access_key_file, &link).expect("symlink");
    let text = config_text(&setup, "127.0.0.1:0", &[("a", "http://127.0.0.1:1")]).replace(
        &setup.access_key_file.display().to_string(),
        &link.display().to_string(),
    );
    let error = parse(&setup, &text).err().expect("must reject symlink");
    assert!(error.contains("regular file"), "unexpected error: {error}");
}

#[test]
fn key_size_limits() {
    let setup = setup("config-size");
    let cases: [(&str, Vec<u8>, Option<&str>); 4] = [
        ("big.key", vec![b'a'; 4097], Some("1..=4096")),
        ("ws.key", b"  \n\t ".to_vec(), Some("non-whitespace")),
        ("empty.key", Vec::new(), Some("non-whitespace")),
        ("max.key", vec![b'a'; 4096], None),
    ];
    for (name, bytes, expected_error) in cases {
        let path = common::write_key_bytes(&setup.scratch, name, &bytes);
        let text = config_text(&setup, "127.0.0.1:0", &[("a", "http://127.0.0.1:1")]).replace(
            &setup.access_key_file.display().to_string(),
            &path.display().to_string(),
        );
        let result = parse(&setup, &text);
        match expected_error {
            Some(needle) => {
                let error = result.err().expect("must reject");
                assert!(error.contains(needle), "for {name}: {error}");
            }
            None => {
                result.expect("4096-byte key must be accepted");
            }
        }
    }
}

#[test]
fn missing_ui_asset_rejected() {
    let setup = setup("config-ui");
    let empty_ui = setup.scratch.join("empty-ui");
    std::fs::create_dir_all(&empty_ui).expect("mkdir");
    let text = config_text(&setup, "127.0.0.1:0", &[("a", "http://127.0.0.1:1")]).replace(
        &setup.ui_dir.display().to_string(),
        &empty_ui.display().to_string(),
    );
    let error = parse(&setup, &text).err().expect("must reject");
    assert!(
        error.contains("muser-dashboard.html"),
        "unexpected error: {error}"
    );
}

#[test]
fn history_defaults_when_the_table_is_absent() {
    let setup = setup("config-history-default");
    let text = config_text(&setup, "127.0.0.1:0", &[("a", "http://127.0.0.1:1")]);
    let config = parse(&setup, &text).expect("config must parse");
    assert!(config.history.enabled, "the history plane is on by default");
    assert_eq!(config.history.retention_days, 7);
    assert_eq!(
        config.history.sample_interval_ms, 1000,
        "the 1 s cadence is the contract, not a tunable"
    );
    assert_eq!(
        config.history.db_path,
        setup.scratch.join("console-history.sqlite"),
        "a relative db_path anchors against the config directory, never the cwd"
    );
}

/// Splices a `[history]` table in between the top-level keys and the
/// `[[instance]]` array, which is where TOML requires it to sit.
fn with_history(setup: &Setup, body: &str) -> String {
    let base = config_text(setup, "127.0.0.1:0", &[("a", "http://127.0.0.1:1")]);
    let (head, instances) = base
        .split_once("\n[[instance]]")
        .expect("config text has an instance array");
    format!("{head}\n[history]\n{body}\n[[instance]]{instances}")
}

#[test]
fn history_table_is_honoured_and_validated() {
    let setup = setup("config-history");

    let text = with_history(
        &setup,
        "db_path = \"store/history.sqlite\"\nretention_days = 30\nenabled = false\n",
    );
    let config = parse(&setup, &text).expect("history table must parse");
    assert!(!config.history.enabled);
    assert_eq!(config.history.retention_days, 30);
    assert_eq!(
        config.history.db_path,
        setup.scratch.join("store/history.sqlite")
    );

    // An absolute path is taken as given.
    let absolute = setup.scratch.join("elsewhere.sqlite");
    let text = with_history(&setup, &format!("db_path = \"{}\"\n", absolute.display()));
    let config = parse(&setup, &text).expect("absolute db_path must parse");
    assert_eq!(config.history.db_path, absolute);

    let text = with_history(&setup, "retention_days = 0\n");
    let error = parse(&setup, &text).err().expect("must reject");
    assert!(error.contains("at least 1"), "unexpected error: {error}");

    // The sample interval is deliberately not a TOML key.
    let text = with_history(&setup, "sample_interval_ms = 50\n");
    parse(&setup, &text)
        .err()
        .expect("history.sample_interval_ms must not be settable from config");

    let text = with_history(&setup, "surprise = true\n");
    parse(&setup, &text)
        .err()
        .expect("unknown history key must reject");
}

// ---------------------------------------------------------------------------
// [[agent]] — phase-4 sidecar exporters

/// Appends `[[agent]]` tables to a config that already has its instances.
fn with_agents(
    setup: &Setup,
    instances: &[(&str, &str)],
    agents: &[(&str, &str, &str, &str)],
) -> String {
    let mut text = config_text(setup, "127.0.0.1:0", instances);
    for (name, base_url, instance, kind) in agents {
        text.push_str(&format!(
            "\n[[agent]]\nname = \"{name}\"\nbase_url = \"{base_url}\"\n\
             instance = \"{instance}\"\nkind = \"{kind}\"\n"
        ));
    }
    text
}

#[test]
fn agents_parse_and_attach_to_their_instance() {
    let setup = setup("config-agent-happy");
    let text = with_agents(
        &setup,
        &[("gx", "http://127.0.0.1:1"), ("mac", "http://127.0.0.1:2")],
        &[
            ("gx-gpu", "http://127.0.0.1:9707/", "gx", "gx10"),
            ("mac-power", "http://127.0.0.1:9708", "mac", "mac"),
        ],
    );
    let config = parse(&setup, &text).expect("agent config must parse");
    assert_eq!(config.agents.len(), 2);
    assert_eq!(config.agents[0].name, "gx-gpu");
    assert_eq!(
        config.agents[0].base_url, "http://127.0.0.1:9707",
        "the trailing slash normalizes away exactly as an instance's does"
    );
    assert_eq!(config.agents[0].authority, "127.0.0.1:9707");
    assert_eq!(config.agents[0].instance, "gx");
    assert_eq!(config.agents[0].kind.as_str(), "gx10");
    assert_eq!(config.agents[1].kind.as_str(), "mac");

    let attached: Vec<&str> = config
        .agents_for("gx")
        .map(|agent| agent.name.as_str())
        .collect();
    assert_eq!(
        attached,
        ["gx-gpu"],
        "agents list under the instance they name"
    );
    assert_eq!(config.agents_for("nobody").count(), 0);
}

#[test]
fn two_agents_of_one_kind_on_one_instance_are_rejected() {
    // Agent series names carry the device index but not the agent, so a
    // second gx10 agent on the same instance would overwrite the first's
    // rows: one machine's readings would vanish and the other's would show
    // under a name that no longer means what it says. Refusing the config
    // is the only answer that does not lose data silently.
    let setup = setup("config-agent-collision");
    let text = with_agents(
        &setup,
        &[("gx", "http://127.0.0.1:1")],
        &[
            ("gx-a", "http://127.0.0.1:9707", "gx", "gx10"),
            ("gx-b", "http://127.0.0.1:9708", "gx", "gx10"),
        ],
    );
    let error = parse(&setup, &text).err().expect("must reject");
    assert!(
        error.contains("gx-b") && error.contains("gx-a") && error.contains("collide"),
        "unexpected error: {error}"
    );

    // One agent of each kind on one instance is the real topology (a GX10
    // prefill node and the Mac it hands off to) and must still parse.
    let text = with_agents(
        &setup,
        &[("gx", "http://127.0.0.1:1")],
        &[
            ("gx-gpu", "http://127.0.0.1:9707", "gx", "gx10"),
            ("gx-host", "http://127.0.0.1:9708", "gx", "mac"),
        ],
    );
    let config = parse(&setup, &text).expect("one agent per kind must parse");
    assert_eq!(config.agents_for("gx").count(), 2);
}

#[test]
fn an_agent_naming_an_unconfigured_instance_is_rejected() {
    // Samples stored under a name no instance has could never be joined
    // against anything, so this is a config error rather than an orphan.
    let setup = setup("config-agent-orphan");
    let text = with_agents(
        &setup,
        &[("gx", "http://127.0.0.1:1")],
        &[("gx-gpu", "http://127.0.0.1:9707", "typo", "gx10")],
    );
    let error = parse(&setup, &text).err().expect("must reject");
    assert!(
        error.contains("'typo'") && error.contains("not a configured"),
        "unexpected error: {error}"
    );
}

#[test]
fn agent_names_kinds_and_urls_are_validated() {
    let setup = setup("config-agent-validate");
    let instances = [("gx", "http://127.0.0.1:1")];

    let long = "x".repeat(65);
    for name in ["bad name", "bad!name", "", long.as_str()] {
        let text = with_agents(
            &setup,
            &instances,
            &[(name, "http://127.0.0.1:9707", "gx", "gx10")],
        );
        let error = parse(&setup, &text).err().expect("must reject");
        assert!(error.contains("[A-Za-z0-9_-]"), "for '{name}': {error}");
    }

    // Duplicate agent names: two agents answering to one name would make the
    // fleet listing and the scrape-state map disagree about which is which.
    let text = with_agents(
        &setup,
        &instances,
        &[
            ("twin", "http://127.0.0.1:9707", "gx", "gx10"),
            ("twin", "http://127.0.0.1:9708", "gx", "gx10"),
        ],
    );
    let error = parse(&setup, &text).err().expect("must reject duplicate");
    assert!(
        error.contains("duplicate agent"),
        "unexpected error: {error}"
    );

    // HTTPS agents use the separate platform-root client; malformed origins
    // are still refused.
    let text = with_agents(
        &setup,
        &instances,
        &[("gx-gpu", "https://127.0.0.1:9707", "gx", "gx10")],
    );
    let config = parse(&setup, &text).expect("https agent must parse");
    assert!(config.agents[0].is_https);
    for (base_url, needle) in [
        ("http://user@127.0.0.1:9707", "userinfo"),
        ("http://127.0.0.1:9707/metrics", "no path"),
        ("ftp://127.0.0.1:9707", "http://"),
    ] {
        let text = with_agents(&setup, &instances, &[("gx-gpu", base_url, "gx", "gx10")]);
        let error = parse(&setup, &text).err().expect("must reject");
        assert!(error.contains(needle), "for {base_url}: {error}");
    }

    for kind in ["gpu", "GX10", ""] {
        let text = with_agents(
            &setup,
            &instances,
            &[("gx-gpu", "http://127.0.0.1:9707", "gx", kind)],
        );
        let error = parse(&setup, &text).err().expect("must reject");
        assert!(
            error.contains("\"gx10\" or \"mac\""),
            "for kind '{kind}': {error}"
        );
    }
}

#[test]
fn an_agent_cannot_be_given_a_key() {
    // The exporters serve no secrets, so the console has nothing to send
    // them. Making that a parse error is what keeps it true: there is no
    // field to put a credential in.
    let setup = setup("config-agent-keyless");
    let mut text = with_agents(
        &setup,
        &[("gx", "http://127.0.0.1:1")],
        &[("gx-gpu", "http://127.0.0.1:9707", "gx", "gx10")],
    );
    text.push_str(&format!(
        "api_key_file = \"{}\"\n",
        setup.api_key_file.display()
    ));
    parse(&setup, &text)
        .err()
        .expect("an [[agent]] must have nowhere to carry a key");
}

#[test]
fn a_config_without_agents_still_parses() {
    let setup = setup("config-agent-absent");
    let text = config_text(&setup, "127.0.0.1:0", &[("gx", "http://127.0.0.1:1")]);
    let config = parse(&setup, &text).expect("agents are optional");
    assert!(config.agents.is_empty());
}

#[test]
fn unknown_config_keys_rejected() {
    let setup = setup("config-unknown");
    let mut text = config_text(&setup, "127.0.0.1:0", &[("a", "http://127.0.0.1:1")]);
    text.push_str("surprise = true\n");
    parse(&setup, &text).err().expect("unknown key must reject");
}
