//! Behavioral tests for the migration carry plane: parsing and writing
//! MCP/env/session/credential manifests against temp-dir fixture trees.
//! Moved here from `pantheon-migration/src/carry_tests.rs`; runs under
//! `cargo test -p pantheon-eval`, not beside the code.
use pantheon_migration::*;
use std::fs;
use std::path::{Path, PathBuf};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pantheon-carry-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

// ===========================================================================
// MCP
// ===========================================================================

#[test]
fn parses_mcp_json_stdio_and_http() {
    let body = r#"{
      "mcpServers": {
        "z-http": { "type": "http", "url": "https://docs.mcp.cloudflare.com/mcp" },
        "a-stdio": { "command": "/usr/bin/mcp-proxy", "args": ["--flag","v"] }
      }
    }"#;
    let servers = parse_mcp_json(body).unwrap();
    assert_eq!(servers.len(), 2);
    // Sorted by name, so the plan output is stable.
    assert_eq!(servers[0].name, "a-stdio");
    assert_eq!(servers[0].transport, "stdio");
    assert_eq!(servers[0].command.as_deref(), Some("/usr/bin/mcp-proxy"));
    assert_eq!(servers[0].args, vec!["--flag", "v"]);
    assert_eq!(servers[1].name, "z-http");
    assert_eq!(servers[1].transport, "http");
    assert_eq!(
        servers[1].url.as_deref(),
        Some("https://docs.mcp.cloudflare.com/mcp")
    );
}

#[test]
fn infers_transport_when_absent() {
    let body = r#"{"mcpServers":{"a":{"url":"https://x/mcp"},"b":{"command":"x"}}}"#;
    let s = parse_mcp_json(body).unwrap();
    assert_eq!(s[0].transport, "http");
    assert_eq!(s[1].transport, "stdio");
}

#[test]
fn an_unsupported_transport_is_a_structured_error() {
    let body = r#"{"mcpServers":{"a":{"type":"carrier-pigeon","command":"x"}}}"#;
    let e = parse_mcp_json(body).unwrap_err();
    assert!(e.code.contains("TRANSPORT"), "{}", e.code);
}

#[test]
fn a_credential_header_is_flagged_but_never_copied() {
    let body = r#"{"mcpServers":{"a":{"url":"https://x/mcp",
        "headers":{"Authorization":"Bearer sk-realsecretvalue12345"}}}}"#;
    let s = parse_mcp_json(body).unwrap();
    assert!(s[0].needs_credentials);
    // The token must not appear anywhere in the serialized result.
    let json = serde_json::to_string(&s[0]).unwrap();
    assert!(!json.contains("sk-realsecretvalue12345"), "{json}");
    assert!(!json.contains("Bearer"), "{json}");
}

#[test]
fn env_values_are_reduced_to_names() {
    let body = r#"{"mcpServers":{"a":{"command":"x",
        "env":{"AWS_MCP_PROXY_PROFILES":"default","MY_TOKEN":"abcdef1234567890"}}}}"#;
    let s = parse_mcp_json(body).unwrap();
    assert_eq!(s[0].requires_env, vec!["MY_TOKEN".to_string()]);
    let json = serde_json::to_string(&s[0]).unwrap();
    assert!(!json.contains("abcdef1234567890"), "{json}");
}

#[test]
fn looks_secret_rejects_non_secrets() {
    assert!(!looks_secret("default"));
    assert!(!looks_secret("https://example.com"));
    assert!(!looks_secret("${SOME_VAR}"));
    assert!(!looks_secret("short"));
    assert!(looks_secret("Bearer sk-abcdefghijklmnop"));
    assert!(looks_secret("a1b2c3d4e5f6a7b8c9d0"));
}

#[test]
fn parses_hermes_mcp_block() {
    let yaml = r#"
model: router
database: { journal_mode: wal }
mcp_servers:
  aws-mcp:
    command: /home/u/.hermes/bin/uvx
    args:
      - mcp-proxy-for-aws@latest
      - https://aws-mcp.us-east-1.api.aws/mcp
    env:
      AWS_REGION: us-east-1
  cloudflare-docs:
    url: https://docs.mcp.cloudflare.com/mcp
other_thing:
  x: 1
"#;
    let s = parse_hermes_mcp(yaml);
    assert_eq!(s.len(), 2, "{s:?}");
    assert_eq!(s[0].name, "aws-mcp");
    assert_eq!(s[0].transport, "stdio");
    assert_eq!(
        s[0].args,
        vec![
            "mcp-proxy-for-aws@latest",
            "https://aws-mcp.us-east-1.api.aws/mcp"
        ]
    );
    assert_eq!(s[1].name, "cloudflare-docs");
    assert_eq!(s[1].transport, "http");
    // The block must terminate at the next column-0 key.
    assert!(s.iter().all(|x| x.name != "other_thing"));
}

#[test]
fn hermes_mcp_absent_is_empty_not_an_error() {
    assert!(parse_hermes_mcp("model: x\n").is_empty());
    assert!(parse_hermes_mcp("").is_empty());
}

#[test]
fn a_bare_list_item_only_becomes_an_arg_under_args() {
    // Regression: the old parser appended every bare `- item` to `args`
    // regardless of which key opened the list, so an `env:` list would be
    // folded into the command line.
    let yaml = r#"
mcp_servers:
  aws-mcp:
    command: /bin/uvx
    args:
      - mcp-proxy-for-aws@latest
      - --metadata
    env:
      - AWS_REGION=us-east-1
      - AWS_DEFAULT_REGION=us-east-1
  other:
    command: /bin/other
    args:
      - only-real-arg
"#;
    let s = parse_hermes_mcp(yaml);
    let aws = s.iter().find(|x| x.name == "aws-mcp").unwrap();
    assert_eq!(
        aws.args,
        vec!["mcp-proxy-for-aws@latest", "--metadata"],
        "env list items leaked into args: {:?}",
        aws.args
    );
    let other = s.iter().find(|x| x.name == "other").unwrap();
    assert_eq!(other.args, vec!["only-real-arg"]);
}

#[test]
fn a_bearer_header_records_the_env_var_name_and_never_the_token() {
    // The real composio entry from a live config.yaml.
    let yaml = r#"
mcp_servers:
  composio:
    url: https://connect.composio.dev/mcp
    connect_timeout: 60
    headers:
      Authorization: Bearer ${MCP_COMPOSIO_API_KEY}
    enabled: true
"#;
    let s = parse_hermes_mcp(yaml);
    assert_eq!(s.len(), 1);
    let c = &s[0];
    assert_eq!(c.transport, "http");
    assert!(c.needs_credentials, "a bearer header is a credential");
    assert_eq!(c.requires_env, vec!["MCP_COMPOSIO_API_KEY".to_string()]);
    let json = serde_json::to_string(c).unwrap();
    assert!(
        !json.contains("Bearer"),
        "the header value must not carry: {json}"
    );
}

#[test]
fn a_literal_token_in_a_header_is_flagged_but_the_name_is_not_invented() {
    let yaml = r#"
mcp_servers:
  leaky:
    url: https://x.example/mcp
    headers:
      X-Api-Key: sk-realsecretvalue12345
"#;
    let c = parse_hermes_mcp(yaml);
    assert!(c[0].needs_credentials);
    // A literal token has no env var name to point at; inventing one would be
    // worse than reporting nothing.
    assert!(c[0].requires_env.is_empty());
    let json = serde_json::to_string(&c[0]).unwrap();
    assert!(!json.contains("sk-realsecretvalue12345"), "{json}");
}

#[test]
fn a_secret_valued_env_entry_is_flagged() {
    let yaml = r#"
mcp_servers:
  a:
    command: /bin/x
    env:
      MY_TOKEN: abcdef1234567890
      AWS_REGION: us-east-1
"#;
    let c = parse_hermes_mcp(yaml);
    assert!(c[0].needs_credentials);
}

#[test]
fn a_flow_sequence_args_list_is_parsed() {
    let yaml = "mcp_servers:\n  a:\n    command: /bin/x\n    args: [one, two, three]\n";
    let c = parse_hermes_mcp(yaml);
    assert_eq!(c[0].args, vec!["one", "two", "three"]);
}

#[test]
fn a_server_with_only_a_url_is_http() {
    let yaml = "mcp_servers:\n  docs:\n    url: https://docs.mcp.cloudflare.com/mcp\n";
    let c = parse_hermes_mcp(yaml);
    assert_eq!(c[0].transport, "http");
    assert!(c[0].command.is_none());
}

#[test]
fn the_real_hermes_mcp_block_parses_correctly() {
    // Verbatim shape from a live ~/.hermes/config.yaml.
    let yaml = r#"
mcp_servers:
  aws-mcp:
    command: /home/u/.hermes/bin/uvx
    args:
      - mcp-proxy-for-aws@latest
      - https://aws-mcp.us-east-1.api.aws/mcp
      - --metadata
      - INSTALL_SOURCE=aws-cli
    env:
      AWS_MCP_PROXY_PROFILES: default
      AWS_REGION: us-east-1
      AWS_DEFAULT_REGION: us-east-1
  cloudflare-docs:
    url: https://docs.mcp.cloudflare.com/mcp
  composio:
    url: https://connect.composio.dev/mcp
    connect_timeout: 60
    headers:
      Authorization: Bearer ${MCP_COMPOSIO_API_KEY}
    enabled: true
  codebase-memory:
    command: /home/u/.local/bin/codebase-memory-mcp
    connect_timeout: 60
    enabled: true
platform_toolsets:
  cli: hermes-cli
"#;
    let s = parse_hermes_mcp(yaml);
    assert_eq!(s.len(), 4, "{s:?}");
    // Sorted by name.
    assert_eq!(
        s.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(),
        vec!["aws-mcp", "cloudflare-docs", "codebase-memory", "composio"]
    );

    let aws = &s[0];
    assert_eq!(aws.transport, "stdio");
    assert_eq!(aws.command.as_deref(), Some("/home/u/.hermes/bin/uvx"));
    assert_eq!(aws.args.len(), 4);

    assert_eq!(s[1].transport, "http");
    assert_eq!(s[2].transport, "stdio");
    assert!(!s[2].needs_credentials);

    let comp = &s[3];
    assert_eq!(comp.transport, "http");
    assert!(comp.needs_credentials);
    assert_eq!(comp.requires_env, vec!["MCP_COMPOSIO_API_KEY".to_string()]);
}

// ===========================================================================
// Credentials
// ===========================================================================

#[test]
fn parses_env_names_without_exposing_values() {
    let body = "# comment\n\
                OPENAI_API_KEY=sk-realsecretvalue12345\n\
                export EXPORTED_THING=abc\n\
                EMPTY=\n\
                YOUR_KEY_HERE=your_actual_key\n\
                QUOTED=\"quotedvalue12345\"\n\
                not a line\n\
                1BAD=x\n";
    let e = parse_env_names(body);
    let names: Vec<&str> = e.iter().map(|x| x.name.as_str()).collect();
    assert!(names.contains(&"OPENAI_API_KEY"));
    assert!(names.contains(&"EXPORTED_THING"));
    assert!(names.contains(&"EMPTY"));
    assert!(names.contains(&"QUOTED"));
    assert!(!names.contains(&"1BAD"), "non-identifier key rejected");
    assert!(
        !names.iter().any(|n| n.contains(' ')),
        "comment/malformed rejected"
    );

    for x in &e {
        assert!(
            x.value.is_none(),
            "parse_env_names must never return values"
        );
    }
    let openai = e.iter().find(|x| x.name == "OPENAI_API_KEY").unwrap();
    assert!(openai.had_value);
    let empty = e.iter().find(|x| x.name == "EMPTY").unwrap();
    assert!(!empty.had_value);
    let placeholder = e.iter().find(|x| x.name == "YOUR_KEY_HERE").unwrap();
    assert!(!placeholder.had_value, "a placeholder is not a credential");
    let quoted = e.iter().find(|x| x.name == "QUOTED").unwrap();
    assert!(quoted.had_value);
}

#[test]
fn parse_env_values_opts_in_explicitly() {
    let e = parse_env_values("K=supersecretvalue\n");
    assert_eq!(e[0].value.as_deref(), Some("supersecretvalue"));
    // And it is still absent when values were not asked for.
    assert!(parse_env_names("K=supersecretvalue\n")[0].value.is_none());
}

#[test]
fn classifies_credentials_by_name_only() {
    assert_eq!(
        classify_credential("OPENAI_API_KEY"),
        CredentialTarget::Provider
    );
    assert_eq!(
        classify_credential("ANTHROPIC_API_KEY"),
        CredentialTarget::Provider
    );
    assert_eq!(
        classify_credential("TELEGRAM_BOT_TOKEN"),
        CredentialTarget::Channel
    );
    assert_eq!(
        classify_credential("DISCORD_BOT_TOKEN"),
        CredentialTarget::Channel
    );
    assert_eq!(
        classify_credential("MCP_COMPOSIO_API_KEY"),
        CredentialTarget::McpAuth
    );
    assert_eq!(classify_credential("HOME"), CredentialTarget::Unclassified);
    assert_eq!(
        classify_credential("AWS_REGION"),
        CredentialTarget::Unclassified
    );
}

#[test]
fn manifest_holds_names_and_never_values() {
    let names = vec!["OPENAI_API_KEY".to_string(), "HOME".to_string()];
    let m = credential_manifest("hermes", &names);
    assert_eq!(m.mappings.len(), 2);
    assert_eq!(m.unclassified(), 1);
    let json = serde_json::to_string(&m).unwrap();
    assert!(json.contains("OPENAI_API_KEY"));
    assert!(!json.contains("sk-"), "no value-shaped content: {json}");
}

#[test]
fn reading_a_dotenv_is_last_wins_per_key() {
    let d = tmp("env-read");
    let p = d.join(".env");
    fs::write(&p, "A=1\nA=2\nB=3\n").unwrap();
    let pairs = read_dotenv(&p);
    assert_eq!(
        pairs,
        vec![("A".into(), "2".into()), ("B".into(), "3".into())]
    );
}

#[test]
fn the_pantheon_env_path_is_the_native_one() {
    assert_eq!(pantheon_env_path(Path::new("/d")), PathBuf::from("/d/.env"));
}

#[test]
fn same_named_transcripts_in_different_subdirs_do_not_collide() {
    // Regression: the quarantine copy flattened to `d{depth}-{name}`, so
    // `sessions/a/t.jsonl` and `sessions/b/t.jsonl` both became `d1-t.jsonl`
    // and the second silently overwrote the first.
    let d = tmp("sess-collide");
    let from = d.join("sessions");
    fs::create_dir_all(from.join("a")).unwrap();
    fs::create_dir_all(from.join("b")).unwrap();
    fs::create_dir_all(from.join("c/d")).unwrap();
    fs::write(from.join("a/t.jsonl"), "{\"who\":\"a\"}\n").unwrap();
    fs::write(from.join("b/t.jsonl"), "{\"who\":\"b\"}\n").unwrap();
    fs::write(from.join("c/d/t.jsonl"), "{\"who\":\"c-d\"}\n").unwrap();

    let imp = write_session_import(&d.join("data"), "hermes", &from).unwrap();
    assert_eq!(
        imp.files.len(),
        3,
        "all three must survive: {:?}",
        imp.files
    );
    let names: Vec<String> = imp
        .files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        names.iter().collect::<std::collections::HashSet<_>>().len(),
        3,
        "{names:?}"
    );

    // And each landed file still holds its own content.
    let mut seen = std::collections::HashSet::new();
    for f in &imp.files {
        let body = fs::read_to_string(f).unwrap();
        seen.insert(body.trim().to_string());
    }
    assert_eq!(seen.len(), 3, "contents were overwritten: {seen:?}");
}

#[test]
fn a_symlinked_transcript_is_not_followed() {
    let d = tmp("sess-symlink");
    let from = d.join("sessions");
    fs::create_dir_all(&from).unwrap();
    let outside = d.join("secret.txt");
    fs::write(&outside, "TOP SECRET\n").unwrap();
    fs::write(from.join("real.jsonl"), "{}\n").unwrap();
    std::os::unix::fs::symlink(&outside, from.join("leak.jsonl")).unwrap();

    let imp = write_session_import(&d.join("data"), "hermes", &from).unwrap();
    assert_eq!(imp.files.len(), 1, "only the real transcript");
    for f in &imp.files {
        assert!(!fs::read_to_string(f).unwrap().contains("TOP SECRET"));
    }
}

// ===========================================================================
// Sessions
// ===========================================================================

#[test]
fn recognises_transcript_formats() {
    let d = tmp("sess-fmt");
    fs::write(d.join("a.jsonl"), "").unwrap();
    fs::write(d.join("b.json"), "").unwrap();
    fs::write(d.join("c.txt"), "").unwrap();
    assert_eq!(transcript_format(&d.join("a.jsonl")), Some("jsonl"));
    assert_eq!(transcript_format(&d.join("b.json")), Some("json"));
    assert_eq!(transcript_format(&d.join("c.txt")), None);
}

#[test]
fn counts_jsonl_records_without_retaining_them() {
    let d = tmp("sess-count");
    let p = d.join("t.jsonl");
    fs::write(&p, "{\"a\":1}\n\n{\"b\":2}\n   \n{\"c\":3}\n").unwrap();
    assert_eq!(count_jsonl_records(&p), 3);
    assert_eq!(count_jsonl_records(&d.join("missing.jsonl")), 0);
}

// ===========================================================================
// MCP declarations
// ===========================================================================

#[test]
fn an_mcp_declaration_round_trips_through_its_own_reader() {
    let d = tmp("mcp-decl");
    let servers = parse_mcp_json(
        r#"{"mcpServers":{"a":{"command":"/bin/x","args":["--f"]},"b":{"url":"https://b/mcp"}}}"#,
    )
    .unwrap();
    let path = write_mcp_declaration(&d, "hermes", &servers).unwrap();
    assert_eq!(path, d.join("mcp/hermes.json"));
    let body = std::fs::read_to_string(&path).unwrap();
    // Readable back: a declaration nobody can read is a dead file.
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["source"], "hermes");
    assert_eq!(v["servers"].as_array().unwrap().len(), 2);
    assert!(body.contains("`pantheon mcp`"), "must say how to use it");
}

#[test]
fn an_empty_server_list_writes_no_file() {
    let d = tmp("mcp-empty");
    let p = write_mcp_declaration(&d, "hermes", &[]).unwrap();
    assert!(p.as_os_str().is_empty());
    assert!(!d.join("mcp/hermes.json").exists(), "no empty artefact");
}

#[test]
fn read_declarations_finds_every_source_written() {
    let d = tmp("mcp-read");
    let one = parse_mcp_json(r#"{"mcpServers":{"a":{"command":"x"}}}"#).unwrap();
    write_mcp_declaration(&d, "hermes", &one).unwrap();
    write_mcp_declaration(&d, "omp", &one).unwrap();
    let all = read_mcp_declarations(&d);
    assert_eq!(all.len(), 2);
    assert!(all.iter().any(|g| g.source == "hermes"));
    assert!(all.iter().any(|g| g.source == "omp"));
}

#[test]
fn a_declaration_reports_a_server_that_still_needs_a_credential() {
    let d = tmp("mcp-needs");
    let servers = parse_mcp_json(
        r#"{"mcpServers":{"a":{"url":"https://a/mcp","headers":{"Authorization":"Bearer sk-abc123456"}},
                         "b":{"command":"x"}}}"#,
    )
    .unwrap();
    write_mcp_declaration(&d, "hermes", &servers).unwrap();
    let all = read_mcp_declarations(&d);
    assert_eq!(all.len(), 1);
    let g = &all[0];
    assert_eq!(g.source, "hermes");
    assert_eq!(g.servers.len(), 2);
    let needing = g.servers.iter().filter(|s| s.needs_credentials).count();
    assert_eq!(needing, 1, "the bearer header must still be flagged");
    // And the literal token is nowhere in the group.
    let json = serde_json::to_string(g).unwrap();
    assert!(!json.contains("sk-abc123456"), "{json}");
}
