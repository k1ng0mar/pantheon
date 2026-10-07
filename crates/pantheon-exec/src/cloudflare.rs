//! Cloudflare command classification for `cf` CLI invocations.
//!
//! The shell tool's capability hook calls [`classify`] on every command that
//! starts with `cf`. The classification decides which extra capabilities the
//! call needs, and the policy machinery (Approval for the write/destroy
//! tokens) parks the run for a human before anything executes. No second
//! approval system: this is classification only, the gate stays in
//! `pantheon-api::capability` + the run loop.
//!
//! Classification is verb-driven on the parsed subcommand path, mirroring how
//! `cf` itself names operations (`cf <product> [group...] <operation>`, the
//! Forge-generated shape). Verb words come from the observed cf surface and
//! the API semantics Cloudflare documents; unknown shapes classify as
//! Destroy (fail closed): a command we cannot read must not ride the cheap
//! path. Pure functions, no I/O, no process spawns.

/// How much a `cf` invocation can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CfOp {
    /// Inspection only: list/get/describe/search/status/whoami and friends.
    Read,
    /// Create or modify a resource. Reversible through another Write,
    /// but not through nothing.
    Write,
    /// Delete, or a change whose wrong application is expensive to undo
    /// (DNS records on proxied zones, nameservers, WAF/firewall rules,
    /// Access policies, zone settings, tokens and secrets).
    Destroy,
}

impl CfOp {
    /// The `Capability::Other` token this operation class maps to, when it
    /// needs one beyond plain `ShellExecute`.
    pub fn capability_token(self) -> Option<&'static str> {
        match self {
            CfOp::Read => None,
            CfOp::Write => Some("cloudflare.write"),
            CfOp::Destroy => Some("cloudflare.destroy"),
        }
    }
}

/// Split a command line into argv the way `sh -c` would for our purposes:
/// whitespace-separated, respecting single and double quotes. No shell
/// semantics beyond quoting (no expansions needed: we only read verb words).
fn split_argv(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in cmd.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if quote.is_some() => {
                escaped = true;
            }
            '\'' | '"' if quote.is_none() => {
                quote = Some(c);
            }
            c if Some(c) == quote => {
                quote = None;
            }
            c if quote.is_none() && c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// True when the token looks like a flag we should skip in the verb path
/// (`--profile work`, `-z example.com` consume a following value).
fn flag_consumes_value(tok: &str) -> bool {
    tok == "--profile"
        || tok == "-z"
        || tok == "--zone"
        || tok == "-m"
        || tok == "--mode"
        || tok == "--persist-to"
        || tok == "--env"
}

/// The operation verbs, checked against the LAST argv word (the operation
/// position in `cf <product> [group...] <operation>`). A product/group word
/// that happens to equal a verb word (`cf dns records scan`) must not read
/// as its operation: only the final path component counts.
fn op_class(word: &str) -> Option<CfOp> {
    let w = word.to_ascii_lowercase();
    match w.as_str() {
        // read-shaped operations
        "list" | "get" | "show" | "describe" | "search" | "status" | "whoami" | "view" | "scan"
        | "scan-list" | "scan-review" | "preview" | "check" | "info" | "events" | "logs"
        | "tail" | "metrics" | "analytics" | "validate" | "verify" | "test" | "help"
        | "complete" | "version" => Some(CfOp::Read),
        // write-shaped operations: create/modify without destruction
        "create" | "update" | "put" | "patch" | "add" | "set" | "import" | "enable" | "disable"
        | "attach" | "detach" | "rename" | "copy" | "migrate" | "init" | "deploy" | "upload"
        | "download" | "purge" | "rotate" | "activate" | "deactivate" | "pause" | "resume"
        | "trigger" | "run" | "start" | "stop" | "restart" | "register" | "bind" | "link"
        | "invite" | "complete-upload" | "push" | "rollback" | "login" | "new" | "edit" | "dev"
        | "scan-trigger" | "generate" | "fork" | "clone" | "sync" | "unlock" | "lock"
        | "restore" | "recover" => Some(CfOp::Write),
        // destroy-shaped operations
        "delete" | "destroy" | "remove" | "drop" | "purge-all" | "reset" | "logout" | "revoke"
        | "deprovision" | "cancel" | "wipe" => Some(CfOp::Destroy),
        _ => None,
    }
}

/// Products whose operations are always Destroy-class regardless of verb:
/// a wrong call here is expensive or irreversible, so anything mutating
/// classifies as Destroy and even a few read-shaped verbs that mutate
/// (`login` writes credentials) are treated with care.
fn product_forces_destroy(path: &[&str]) -> bool {
    const DESTROY_PRODUCTS: &[&str] = &[
        // DNS on live zones: a bad record takes the domain's traffic down.
        "dns",
        // Nameserver / registrar / zone lifecycle.
        "registrar",
        "zones",
        "zone",
        // Firewall, WAF, rulesets, rate limiting.
        "firewall",
        "waf",
        "rulesets",
        "rate-limits",
        "ruleset",
        // Zero trust / identity / access policy.
        "access",
        "gateway",
        "warp",
        "identity",
        "zero-trust",
        "teams",
        // Tokens, secrets, certificates: credential material.
        "tokens",
        "token",
        "secrets",
        "certificates",
        "ssl",
        "mtls",
        // Account-wide / billing operations.
        "billing",
        "subscriptions",
        "members",
    ];
    path.iter()
        .any(|p| DESTROY_PRODUCTS.contains(&p.to_ascii_lowercase().as_str()))
}

/// Classify one `cf ...` command line.
///
/// The operation verb is the LAST PATH COMPONENT, but `cf` commands carry
/// positional arguments after the operation (`cf kv namespace create
/// prod-cache`), so the verb hunt walks the path from the end and skips
/// words that cannot be verbs (anything containing a slash, a dot, or an
/// `=` looks like an id/zone/record argument). Flags and their values are
/// skipped first. Unknown operations classify Destroy: fail closed.
pub fn classify(cmd: &str) -> CfOp {
    let argv = split_argv(cmd);
    // argv[0] must be `cf` for this classification to apply at all; the
    // caller guarantees that, but a defensive check keeps this pure.
    if argv.first().map(|a| a.as_str()) != Some("cf") {
        return CfOp::Destroy;
    }
    // Skip global flags and their values so `cf -z zone.com dns records
    // list` reads the verb path, not the flag values.
    let mut path: Vec<String> = Vec::new();
    let mut skip_next = false;
    for tok in &argv[1..] {
        if skip_next {
            skip_next = false;
            continue;
        }
        if flag_consumes_value(tok) {
            skip_next = true;
            continue;
        }
        if tok.starts_with('-') {
            continue;
        }
        path.push(tok.clone());
    }
    if path.is_empty() {
        // Bare `cf` / `cf --help`: help text only.
        return CfOp::Read;
    }
    let refs: Vec<&str> = path.iter().map(String::as_str).collect();
    // `cf cli search "<anything>"`: the whole query is one argv word and
    // its verbs are not operations. The command itself is a lookup.
    if refs
        .iter()
        .any(|p| *p == "search" || *p == "help" || *p == "complete")
    {
        return CfOp::Read;
    }
    // Hunt the operation verb from the end of the path. Words that look
    // like values (ids, domains, file paths, key=value) are not verbs.
    let looks_like_value =
        |w: &str| w.contains('/') || w.contains('.') || w.contains('=') || w.contains(':');
    let mut op = None;
    for w in refs.iter().rev() {
        if looks_like_value(w) {
            continue;
        }
        if let Some(c) = op_class(w) {
            op = Some(c);
            break;
        }
        // A non-verb, non-value word at the END (a bare argument like
        // `prod-cache`) is an argument: keep walking. But the FIRST word
        // (the product) is never the operation; stop there.
    }
    let op = op.unwrap_or(CfOp::Destroy);
    // Escalations, never de-escalations: the strictest class wins.
    let mut class = op;
    if product_forces_destroy(&refs) {
        if class != CfOp::Read {
            class = CfOp::Destroy;
        }
    } else if class == CfOp::Read && product_forces_write_at_product(&refs) {
        class = CfOp::Write;
    }
    class
}

/// Product-position escalation only: `cf auth login` writes credentials,
/// but `cf auth whoami` must stay Read. The product word is the FIRST
/// path element (or the second for the bare `cf auth login` shape, which
/// is the same thing here).
fn product_forces_write_at_product(path: &[&str]) -> bool {
    const WRITE_PRODUCTS: &[&str] = &["init", "deploy", "migrate"];
    path.iter()
        .any(|p| WRITE_PRODUCTS.contains(&p.to_ascii_lowercase().as_str()))
}

/// True when the command line is a `cf` invocation this module handles.
pub fn is_cf_command(cmd: &str) -> bool {
    let argv = split_argv(cmd);
    peel_to_command(&argv).first().map(|a| a.as_str()) == Some("cf")
}

/// Wrappers whose next word is the command.
const FLAG_ONLY_WRAPPERS: &[&str] = &[
    "sudo", "env", "nohup", "setsid", "time", "builtin", "command", "exec",
];

/// Wrappers that take flags (some with values) and then one operand
/// before the command.
const OPERAND_WRAPPERS: &[&str] = &["timeout", "nice", "stdbuf"];

/// Flags that consume the following word as their value.
const VALUE_FLAGS: &[&str] = &[
    "-n",
    "--adjustment",
    "-o",
    "--output",
    "-i",
    "--input",
    "-e",
    "--error",
];

/// Drop leading wrappers and assignments so the caller sees the real
/// command word.
///
/// This must agree with the wrapper handling in `danger.rs`: when the two
/// disagree, a wrapped command slips past classification. It used to peel
/// only `env`/`sudo`/`nohup`, so `timeout 30 cf workers delete` was not
/// recognized as a cf command at all. The destroy classification never
/// ran, no `cloudflare.destroy` capability was required, and the delete
/// proceeded unattended under the coder policy. Same for `setsid`,
/// `exec`, `command`, and `nice -n 5`.
fn peel_to_command(argv: &[String]) -> Vec<String> {
    let mut rest = argv.to_vec();
    loop {
        let Some(first) = rest.first().cloned() else {
            return Vec::new();
        };
        // Uppercase-only `NAME=value` assignments. A bare `=` or a
        // lowercase name is a shell word, not an assignment.
        let is_assign = first.contains('=')
            && first
                .split('=')
                .next()
                .map(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
                .unwrap_or(false);
        if is_assign || FLAG_ONLY_WRAPPERS.contains(&first.as_str()) {
            rest.remove(0);
            continue;
        }
        if OPERAND_WRAPPERS.contains(&first.as_str()) {
            rest.remove(0);
            // Its own flags. Two shapes matter:
            //   `-n 5`  the flag takes the NEXT word as its value
            //   `-o0`   the value is attached to the flag
            // Consuming a following word that is not a flag is what
            // handled `timeout 30`, but it must only happen for the
            // split form, otherwise `cf` gets eaten as `nice`'s operand.
            let mut consumed_value = false;
            while rest.first().map(|a| a.starts_with('-')).unwrap_or(false) {
                let flag = rest[0].clone();
                if VALUE_FLAGS.contains(&flag.as_str()) {
                    rest.remove(0);
                    if !rest.is_empty() {
                        rest.remove(0);
                        consumed_value = true;
                    }
                } else if flag.len() > 2
                    && VALUE_FLAGS.contains(&flag[..2].as_ref())
                    && flag[2..].chars().all(|c| !c.is_ascii_alphabetic())
                {
                    // Attached value: `-o0`, `-n5`.
                    rest.remove(0);
                    consumed_value = true;
                } else {
                    rest.remove(0);
                }
            }
            // A bare operand, the `timeout 30` form, only when no flag
            // already supplied the value.
            if !consumed_value && !rest.is_empty() && !rest[0].starts_with('-') {
                rest.remove(0);
            }
            continue;
        }
        return rest;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A destroy verb must stay classified as a destroy through every
    /// wrapper. `is_cf_command` used to peel only `env`/`sudo`/`nohup`,
    /// so `timeout 30 cf workers delete` was not seen as a cf command at
    /// all: `cloudflare.destroy` was never required and the delete ran
    /// unattended under the coder policy.
    ///
    /// This walks the wrappers rather than spot-checking one, because the
    /// failure mode is a wrapper quietly missing from the list.
    #[test]
    fn destroy_survives_every_wrapper() {
        const WRAPPERS: &[&str] = &[
            "sudo",
            "env FOO=1",
            "nohup",
            "setsid",
            "command",
            "exec",
            "timeout 30",
            "nice -n 5",
            "stdbuf -o0",
            "sudo timeout 30",
        ];
        for w in WRAPPERS {
            let cmd = format!("{w} cf workers delete x");
            assert!(is_cf_command(&cmd), "wrapper lost the cf prefix: {cmd}");
            assert_eq!(
                classify(&cmd).capability_token(),
                Some("cloudflare.destroy"),
                "wrapper dropped the destroy classification: {cmd}"
            );
        }
    }

    /// `xargs` takes its command from stdin, not from argv, so a `cf`
    /// word in the argument list is not a cf invocation. Treating it as
    /// one would demand approval for unrelated commands.
    #[test]
    fn xargs_argv_is_not_a_cf_command() {
        assert!(!is_cf_command("xargs cf workers delete x"));
    }

    /// A lowercase `name=value` is a shell word, not an assignment, so
    /// it must not be peeled and mistaken for a wrapper prefix.
    #[test]
    fn lowercase_assignment_is_not_peeled() {
        assert!(!is_cf_command("foo=bar cf workers delete x"));
    }

    #[test]
    fn reads_classify_read() {
        for cmd in [
            "cf zones list",
            "cf dns records list --zone exoseed.africa",
            "cf auth whoami",
            "cf cli search \"point a domain at a server\"",
            "cf workers scripts list",
            "cf -z exoseed.africa dns records get abc123",
            "cf r2 bucket list",
            "cf accounts list",
        ] {
            assert_eq!(classify(cmd), CfOp::Read, "{cmd}");
        }
    }

    #[test]
    fn writes_classify_write() {
        for cmd in [
            "cf kv namespace create prod-cache",
            "cf workers deployments rollback",
            "cf r2 object upload --bucket assets --file x.png",
            "cf ai-gateway logpush create",
        ] {
            assert_eq!(classify(cmd), CfOp::Write, "{cmd}");
        }
    }

    #[test]
    fn deletes_classify_destroy() {
        for cmd in [
            "cf kv namespace delete prod-cache",
            "cf workers script delete worker-name",
            "cf r2 object delete --bucket assets --key x.png",
            "cf accounts subscriptions cancelDelayedDowngrade",
        ] {
            assert_eq!(classify(cmd), CfOp::Destroy, "{cmd}");
        }
    }

    #[test]
    fn dns_mutations_are_destroy_even_when_the_verb_is_write() {
        assert_eq!(
            classify("cf dns records create --zone exoseed.africa ..."),
            CfOp::Destroy
        );
        assert_eq!(
            classify("cf dns records update rec-id --content 1.2.3.4"),
            CfOp::Destroy
        );
        // A DNS read stays read.
        assert_eq!(classify("cf dns records list"), CfOp::Read);
        assert_eq!(
            classify("cf dns records import"),
            CfOp::Destroy,
            "bulk DNS import mutates the zone"
        );
    }

    #[test]
    fn waf_access_tokens_and_zones_are_destroy_products() {
        for cmd in [
            "cf firewall rules create",
            "cf waf ruleset update",
            "cf access application create",
            "cf tokens create",
            "cf zone setting update",
            "cf ssl certificate create",
        ] {
            assert_eq!(classify(cmd), CfOp::Destroy, "{cmd}");
        }
    }

    #[test]
    fn auth_writes_are_write_and_auth_read_is_read() {
        assert_eq!(classify("cf auth login"), CfOp::Write);
        assert_eq!(classify("cf auth logout"), CfOp::Destroy);
        assert_eq!(classify("cf auth whoami"), CfOp::Read);
        assert_eq!(classify("cf auth list"), CfOp::Read);
    }

    #[test]
    fn unknown_verbs_fail_closed() {
        assert_eq!(classify("cf something-unknown do-a-thing"), CfOp::Destroy);
        assert_eq!(classify("cf"), CfOp::Read, "bare cf prints help");
    }

    #[test]
    fn quotes_and_flags_do_not_confuse_the_parser() {
        assert_eq!(
            classify("cf cli search 'delete a dns record'"),
            CfOp::Read,
            "the verb inside a search string is not the operation"
        );
        assert_eq!(classify("cf --profile work zones list"), CfOp::Read);
    }

    #[test]
    fn is_cf_command_handles_env_prefix() {
        assert!(is_cf_command("cf zones list"));
        assert!(is_cf_command("CLOUDFLARE_API_TOKEN=x cf zones list"));
        assert!(is_cf_command("env cf zones list"));
        assert!(!is_cf_command("cfly build"));
        assert!(!is_cf_command("ls"));
    }

    #[test]
    fn split_argv_respects_quotes() {
        assert_eq!(
            split_argv(r#"cf dns records create --name 'api.example.com'"#),
            vec![
                "cf",
                "dns",
                "records",
                "create",
                "--name",
                "api.example.com"
            ]
        );
    }

    /// Classifications pinned to commands verified against the real cf
    /// surface (`cf cli search`, 2026-10-02, cf v1.0.0-beta.12). When cf
    /// renames verbs these fail and the table needs a look, not a guess.
    #[test]
    fn live_surface_commands_classify() {
        // Real search results.
        assert_eq!(classify("cf workers delete"), CfOp::Destroy);
        assert_eq!(classify("cf dns records create"), CfOp::Destroy);
        assert_eq!(
            classify("cf dns records update rec-id --content 1.2.3.4"),
            CfOp::Destroy
        );
        assert_eq!(classify("cf dns records edit rec-id"), CfOp::Destroy);
        assert_eq!(classify("cf cache purge"), CfOp::Write);
        assert_eq!(
            classify("cf zero-trust access service-tokens rotate"),
            CfOp::Destroy
        );
        assert_eq!(
            classify("cf zero-trust access applications create"),
            CfOp::Destroy
        );
        assert_eq!(classify("cf firewall access-rules delete"), CfOp::Destroy);
        assert_eq!(classify("cf workers list"), CfOp::Read);
        assert_eq!(classify("cf workers versions list"), CfOp::Read);
        assert_eq!(classify("cf r2 buckets list"), CfOp::Read);
        assert_eq!(classify("cf zones settings get"), CfOp::Read);
        assert_eq!(classify("cf pages deployments delete"), CfOp::Destroy);
        assert_eq!(classify("cf kv keys put"), CfOp::Write);
        assert_eq!(classify("cf kv bulk delete"), CfOp::Destroy);
        assert_eq!(classify("cf builds deploy-hooks trigger"), CfOp::Write);
        assert_eq!(classify("cf deploy"), CfOp::Write);
        assert_eq!(classify("cf dev"), CfOp::Write);
        assert_eq!(classify("cf migrate"), CfOp::Write);
    }

    #[test]
    fn capability_tokens_map() {
        assert_eq!(CfOp::Read.capability_token(), None);
        assert_eq!(CfOp::Write.capability_token(), Some("cloudflare.write"));
        assert_eq!(CfOp::Destroy.capability_token(), Some("cloudflare.destroy"));
    }
}
