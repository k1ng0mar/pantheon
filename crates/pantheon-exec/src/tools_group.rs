//! Exec tools_group merged group.

pub mod cloudflare {
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
            "list" | "get" | "show" | "describe" | "search" | "status" | "whoami" | "view"
            | "scan" | "scan-list" | "scan-review" | "preview" | "check" | "info" | "events"
            | "logs" | "tail" | "metrics" | "analytics" | "validate" | "verify" | "test"
            | "help" | "complete" | "version" => Some(CfOp::Read),
            // write-shaped operations: create/modify without destruction
            "create" | "update" | "put" | "patch" | "add" | "set" | "import" | "enable"
            | "disable" | "attach" | "detach" | "rename" | "copy" | "migrate" | "init"
            | "deploy" | "upload" | "download" | "purge" | "rotate" | "activate" | "deactivate"
            | "pause" | "resume" | "trigger" | "run" | "start" | "stop" | "restart"
            | "register" | "bind" | "link" | "invite" | "complete-upload" | "push" | "rollback"
            | "login" | "new" | "edit" | "dev" | "scan-trigger" | "generate" | "fork" | "clone"
            | "sync" | "unlock" | "lock" | "restore" | "recover" => Some(CfOp::Write),
            // destroy-shaped operations
            "delete" | "destroy" | "remove" | "drop" | "purge-all" | "reset" | "logout"
            | "revoke" | "deprovision" | "cancel" | "wipe" => Some(CfOp::Destroy),
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
    /// The operation verb is normally the last path component, but `cf`
    /// commands carry positional arguments after the operation (`cf kv
    /// namespace create prod-cache`), and an argument can itself be a verb
    /// word (`cf kv namespace delete search`). The verb hunt therefore
    /// takes the STRICTEST verb anywhere in the path, not the last one:
    /// a trailing argument named like a verb escalates instead of
    /// downgrading. Words that look like values (anything containing a
    /// slash, a dot, or an `=` looks like an id/zone/record argument)
    /// are not verbs. Flags and their values are skipped first. Unknown
    /// operations classify Destroy: fail closed.
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
        // Meta-commands whose trailing words are NOT operations, so a
        // verb word inside them must never classify the call:
        // - `cf cli search "<anything>"`: the whole query is free text.
        // - `cf help <topic>` / `cf complete <shell>`: the trailing word
        //   is a help topic / shell name, not an operation.
        // Scoped to these shapes on purpose: the old check returned Read
        // whenever ANY path word was search/help/complete, so
        // `cf kv namespace delete search` - delete a namespace literally
        // named "search" - rode the read tier with no approval.
        if refs
            .first()
            .map(|p| p.eq_ignore_ascii_case("help"))
            .unwrap_or(false)
            || refs
                .first()
                .map(|p| p.eq_ignore_ascii_case("complete"))
                .unwrap_or(false)
            || (refs.len() >= 2
                && refs[0].eq_ignore_ascii_case("cli")
                && refs[1].eq_ignore_ascii_case("search"))
        {
            return CfOp::Read;
        }
        // Hunt the operation verb. A trailing bare argument can itself be
        // a verb word (`delete search`: the namespace is named "search"),
        // so the FIRST verb from the end is not authoritative: the
        // strictest verb anywhere in the path wins. Over-approval when
        // an argument happens to be a verb word (e.g. a key named
        // "delete") is the fail-closed direction.
        let looks_like_value =
            |w: &str| w.contains('/') || w.contains('.') || w.contains('=') || w.contains(':');
        let mut op: Option<CfOp> = None;
        for w in refs.iter() {
            if looks_like_value(w) {
                continue;
            }
            if let Some(c) = op_class(w) {
                op = Some(match (op, c) {
                    (None, c) => c,
                    (Some(CfOp::Destroy), _) | (_, CfOp::Destroy) => CfOp::Destroy,
                    (Some(CfOp::Write), _) | (_, CfOp::Write) => CfOp::Write,
                    (Some(CfOp::Read), CfOp::Read) => CfOp::Read,
                });
            }
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
        fn verb_words_in_argument_position_do_not_downgrade() {
            // A namespace literally named "search" (or "help") is an
            // argument, not the operation: the delete must still classify
            // Destroy. The old any-word search/help/complete early return
            // let these ride the read tier.
            assert_eq!(classify("cf kv namespace delete search"), CfOp::Destroy);
            assert_eq!(classify("cf kv namespace delete help"), CfOp::Destroy);
            assert_eq!(classify("cf workers script delete complete"), CfOp::Destroy);
            // ...while the real meta-commands stay read.
            assert_eq!(classify("cf help delete"), CfOp::Read);
            assert_eq!(classify("cf cli search delete dns"), CfOp::Read);
            assert_eq!(classify("cf complete bash"), CfOp::Read);
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
}
pub mod http {
    //! Lenient HTTP fetching for servers and networks with sloppy TLS.
    //!
    //! Two layers of tolerance:
    //!
    //! 1. **Unclean TLS shutdowns.** Some servers close the TCP connection
    //!    without sending a TLS `close_notify` alert. rustls treats that as
    //!    an `UnexpectedEof` I/O error once the buffered plaintext is
    //!    consumed; curl and OpenSSL accept the bytes. Since every byte we
    //!    keep arrived in an authenticated TLS record - only the *shutdown*
    //!    was sloppy - we accept the body when the framing says it is
    //!    already whole, and keep failing hard on genuine truncation.
    //!
    //! 2. **curl fallback.** Some egress proxies drop rustls handshakes
    //!    while OpenSSL succeeds (observed: every HTTPS host through a
    //!    filtering proxy fails pre-status-line with ureq, while curl
    //!    returns 200). rustls exposes no knob for this - there is no
    //!    response to salvage - so on a transport-level ureq failure we
    //!    retry once via the `curl` CLI (argv-only, no shell) with the same
    //!    timeout/cap posture. HTTP error statuses never trigger it.

    use std::io::{self, Read};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    /// Outcome of an HTTP GET: status plus the body (already cap-checked).
    #[derive(Debug)]
    pub struct Fetched {
        pub status: u16,
        pub body: Vec<u8>,
    }

    /// Crate-neutral GET failure.
    #[derive(Debug)]
    pub enum GetError {
        /// The server answered with a non-2xx status.
        Status(u16),
        /// Transport failure on every attempt (message already cleaned).
        Transport(String),
        TooLarge,
    }

    /// GET `url` with `timeout` and a `max_bytes` body cap.
    ///
    /// Primary path is ureq with the tolerant body reader; on a
    /// transport-level ureq failure it falls back once to `curl`
    /// ([`curl_get`]). HTTP error statuses and oversize bodies never
    /// trigger the fallback.
    pub fn get(
        url: &str,
        timeout: Duration,
        max_bytes: u64,
        user_agent: &str,
    ) -> Result<Fetched, GetError> {
        match get_ureq(url, timeout, max_bytes, user_agent) {
            ok @ Ok(_) => ok,
            err @ Err(GetError::Status(_)) => err,
            err @ Err(GetError::TooLarge) => err,
            Err(GetError::Transport(raw)) => match curl_get(url, timeout, max_bytes, user_agent) {
                Some(r) => r,
                // curl unavailable: report the original ureq failure, cleaned.
                None => Err(GetError::Transport(fetch_error_message(url, &raw))),
            },
        }
    }

    /// GET via ureq only - no curl fallback. Used by callers that must not
    /// inherit the fallback (e.g. paths that need the raw ureq response
    /// handling). Most callers want [`get`].
    fn get_ureq(
        url: &str,
        timeout: Duration,
        max_bytes: u64,
        user_agent: &str,
    ) -> Result<Fetched, GetError> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent();
        let resp = agent
            .get(url)
            .header("User-Agent", user_agent)
            .call()
            .map_err(|e| GetError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        // Statuses are errors here (not transport failures) so the curl
        // fallback below never runs for a server that answered.
        if (400..600).contains(&status) {
            return Err(GetError::Status(status));
        }
        let body =
            read_body_capped(resp, max_bytes).map_err(|e| GetError::Transport(e.to_string()))?;
        if body.len() as u64 > max_bytes {
            return Err(GetError::TooLarge);
        }
        Ok(Fetched { status, body })
    }

    /// Fallback GET via the `curl` CLI: argv-only (no shell), the URL as a
    /// single argv after `--`. Returns `None` when curl is not installed so
    /// the caller can report the original failure instead.
    fn curl_get(
        url: &str,
        timeout: Duration,
        max_bytes: u64,
        user_agent: &str,
    ) -> Option<Result<Fetched, GetError>> {
        let curl = find_curl()?;
        let seq = CURL_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tag = format!("pantheon-curl-{}-{seq}", std::process::id());
        let dir = std::env::temp_dir();
        let hdr_path = dir.join(format!("{tag}.hdr"));
        let body_path = dir.join(format!("{tag}.body"));
        let out = std::process::Command::new(&curl)
            .args([
                "-sS",
                "-L",
                "--max-redirs",
                "5",
                "--max-time",
                &timeout.as_secs().max(1).to_string(),
                "--max-filesize",
                &(max_bytes + 1).to_string(),
                "-A",
                user_agent,
                "--dump-header",
                hdr_path.to_str()?,
                "--output",
                body_path.to_str()?,
                "--",
                url,
            ])
            .output()
            .ok();
        // Always clean up the temp files, success or not.
        let hdr = std::fs::read(&hdr_path).unwrap_or_default();
        let _ = std::fs::remove_file(&hdr_path);
        let body = std::fs::read(&body_path).unwrap_or_default();
        let _ = std::fs::remove_file(&body_path);
        let out = out?;
        if out.status.code() == Some(63) {
            // CURLE_FILESIZE_EXCEEDED: the cap tripped inside curl.
            return Some(Err(GetError::TooLarge));
        }
        let status = parse_status(&hdr);
        match (out.status.success(), status) {
            (true, Some(code @ 200..=299)) => {
                if body.len() as u64 > max_bytes {
                    Some(Err(GetError::TooLarge))
                } else {
                    Some(Ok(Fetched { status: code, body }))
                }
            }
            (_, Some(code)) => Some(Err(GetError::Status(code))),
            _ => {
                let detail = String::from_utf8_lossy(&out.stderr);
                let detail: String = detail
                    .trim()
                    .lines()
                    .last()
                    .unwrap_or("curl failed")
                    .chars()
                    .take(200)
                    .collect();
                Some(Err(GetError::Transport(fetch_error_message(url, &detail))))
            }
        }
    }

    static CURL_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    /// Locate the `curl` binary on PATH.
    fn find_curl() -> Option<PathBuf> {
        let name = if cfg!(windows) { "curl.exe" } else { "curl" };
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths).find_map(|dir| {
                let p = dir.join(name);
                if p.is_file() {
                    Some(p)
                } else {
                    None
                }
            })
        })
    }

    /// Final status code from a `--dump-header` file: the last `HTTP/x`
    /// status line wins (redirect hops come first).
    fn parse_status(hdr: &[u8]) -> Option<u16> {
        String::from_utf8_lossy(hdr)
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                match (parts.next(), parts.next()) {
                    (Some(proto), Some(code)) if proto.starts_with("HTTP/") => code.parse().ok(),
                    _ => None,
                }
            })
            .next_back()
    }

    /// True when `msg` is rustls's "peer closed connection without sending
    /// TLS close_notify" text (surfaced through ureq as a transport error).
    /// rustls exposes no structured variant for this; the `close_notify`
    /// marker is the stable part of its `UNEXPECTED_EOF_MESSAGE`.
    pub fn is_unclean_tls_close_text(msg: &str) -> bool {
        msg.contains("close_notify")
    }

    fn is_unclean_tls_close(e: &io::Error) -> bool {
        e.kind() == io::ErrorKind::UnexpectedEof && is_unclean_tls_close_text(&e.to_string())
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Framing {
        /// Declared byte count; EOF before it arrives is truncation.
        Length(u64),
        /// Terminal chunk ends the body; an unclean close means the
        /// terminal chunk never arrived.
        Chunked,
        /// EOF is the only terminator.
        CloseDelimited,
    }

    fn framing(content_length: Option<&str>, transfer_encoding: Option<&str>) -> Framing {
        if let Some(len) = content_length.and_then(|v| v.parse::<u64>().ok()) {
            return Framing::Length(len);
        }
        let chunked = transfer_encoding
            .map(|v| v.to_ascii_lowercase().contains("chunked"))
            .unwrap_or(false);
        if chunked {
            Framing::Chunked
        } else {
            Framing::CloseDelimited
        }
    }

    /// Decide whether an unclean TLS shutdown (TCP EOF without
    /// `close_notify`) is benign: only when the framing says the body is
    /// already whole. Pure decision logic, unit-tested.
    fn teardown_is_benign(framing: Framing, received: u64) -> bool {
        match framing {
            Framing::Chunked => false,
            Framing::Length(len) => received == len,
            Framing::CloseDelimited => received > 0,
        }
    }

    /// Read a response body, capped at `max_bytes + 1` bytes (the caller
    /// decides what "too large" means).
    ///
    /// Tolerates an unclean TLS shutdown when the framing says the body is
    /// already whole (close-delimited bodies with bytes received, or a
    /// fully-received declared length). Anything else - zero bytes, a short
    /// length-framed body, a chunked body - still errors: that is genuine
    /// truncation, not server sloppiness.
    pub fn read_body_capped(
        mut resp: ureq::http::Response<ureq::Body>,
        max_bytes: u64,
    ) -> io::Result<Vec<u8>> {
        let framing = framing(
            resp.headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok()),
            resp.headers()
                .get("transfer-encoding")
                .and_then(|v| v.to_str().ok()),
        );
        read_capped_tolerant(resp.body_mut().as_reader(), max_bytes, framing)
    }

    fn read_capped_tolerant<R: Read>(
        reader: R,
        max_bytes: u64,
        framing: Framing,
    ) -> io::Result<Vec<u8>> {
        let mut reader = reader.take(max_bytes + 1);
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if is_unclean_tls_close(&e)
                        && teardown_is_benign(framing, buf.len() as u64) =>
                {
                    break
                }
                Err(e) => return Err(e),
            }
        }
        Ok(buf)
    }

    /// User-facing message for a failed fetch. An unclean TLS shutdown gets a
    /// plain-English explanation instead of leaking rustls's internal doc
    /// link into API error bodies; anything else passes through unchanged.
    pub fn fetch_error_message(what: &str, err: &str) -> String {
        if is_unclean_tls_close_text(err) {
            format!(
                "{what}: the server closed the connection without a clean TLS shutdown \
             (it skipped the TLS closing handshake - a server-side quirk); \
             no complete response was received"
            )
        } else {
            format!("{what}: {err}")
        }
    }
}
pub mod lsp {
    //! LSP diagnostics: minimal language-server client for feeding type-check
    //! and lint results back to the model after edits.
    //!
    //! Pantheon edits files through `safewrite`/tools but the model never sees
    //! whether the code still compiles. This module closes that loop: it
    //! speaks the Language Server Protocol over a JSON-RPC stdio connection,
    //! opens a document, pushes changes, and collects `publishDiagnostics`
    //! notifications into a cache that a `lsp.diagnostics` tool reads back.
    //!
    //! What it deliberately is *not*: a full LSP client. It implements the
    //! smallest subset that gets real diagnostics - the `initialize`
    //! handshake, `initialized`, `textDocument/didOpen`, `didChange`,
    //! `shutdown`/`exit` - plus a passive read loop that turns server
    //! notifications into cache entries. Anything richer (completion,
    //! references, code actions) is out of scope here; this is the
    //! diagnostics-only path that unblocks "did my edit break the build".
    use crate::sandbox::{build_sandboxed_checked, SandboxProfile};
    use pantheon_api::error::{Layer, PantheonError};
    use serde::{Deserialize, Serialize};
    use serde_json::Value;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn err(code: &str, cause: String, remediation: &str) -> PantheonError {
        PantheonError::new(code, Layer::Execution, false, cause, remediation, "")
    }

    /// One diagnostic, flattened to the fields the model actually uses.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Diagnostics {
        /// URI of the file the diagnostics apply to.
        pub uri: String,
        /// The server that produced them (for display).
        pub server: String,
        /// Count of diagnostics, newest-first ordering preserved.
        pub count: usize,
        /// Human-readable lines: `[severity] line:col message`.
        pub lines: Vec<String>,
        /// Unix ms when this batch was last updated.
        pub updated_ms: i64,
    }

    /// Shared diagnostics cache, updated by the read thread and read by the
    /// tool. Guarded by a mutex; the read loop is the only writer besides
    /// `open`, so contention is low.
    #[derive(Default)]
    pub struct LspState {
        /// Cache keyed by file URI, newest-first diagnostics per file.
        pub diagnostics: HashMap<String, Diagnostics>,
        /// Pending response, set when the reader thread completes the
        /// handshake's `initialize` response. Cleared by `request` after
        /// delivery. A `None` here means "no response yet".
        pub pending_response: Option<Result<Value, String>>,
    }

    /// A running language-server connection plus its reader thread.
    pub struct LspClient {
        child: Mutex<Child>,
        stdin: Mutex<ChildStdin>,
        next_id: Mutex<u64>,
        state: Arc<Mutex<LspState>>,
        server: String,
        /// The reader thread's join handle, wrapped in a `Mutex` so `shutdown`
        /// (which takes `&self` so it can be called through an `Arc`) can take
        /// it out. A `Mutex` rather than a `RefCell` keeps `LspClient` `Sync`,
        /// so the tool layer can share one live client across its `Send + Sync`
        /// closures.
        reader: Mutex<Option<std::thread::JoinHandle<()>>>,
        /// Set when the reader thread saw the server exit; `request` checks
        /// it so a dead server surfaces as a clean error, not a hang.
        server_alive: Arc<std::sync::atomic::AtomicBool>,
    }

    /// Working directory for a sandboxed language-server spawn: the path the
    /// `root_uri` names when it is an existing absolute directory, otherwise
    /// the process cwd. The namespace wrappers `--chdir` into this, so it has
    /// to be absolute; falling back to the process cwd keeps a synthetic root
    /// (a URI whose directory was never created) from failing the spawn.
    fn cwd_for_root(root_uri: &str) -> String {
        let path = root_uri.strip_prefix("file://").unwrap_or("");
        let p = Path::new(path);
        if p.is_absolute() && p.is_dir() {
            return p.to_string_lossy().into_owned();
        }
        std::env::current_dir()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".to_string())
    }

    impl LspClient {
        /// Start a language server and run the `initialize`/`initialized`
        /// handshake. Fails hard when the server cannot start or does not
        /// answer the handshake within `timeout`.
        ///
        /// `profile` is the sandbox level the server runs at, and it is the
        /// whole point of the argument: an LSP server is model-triggered
        /// process execution, so it must not be the one spawn left outside
        /// the sandbox. `Some(profile)` routes through
        /// [`build_sandboxed_checked`], which refuses (`SANDBOX_UNAVAILABLE`)
        /// when the boundary's wrapper cannot initialize and the direct
        /// fallback is not opted in, and announces an opted-in degraded spawn
        /// on stderr - the same gate the shell tool, the MCP client and the
        /// Python plugin runner use. `None` is the caller-gated exception and
        /// inherits the process cwd exactly as this function always did.
        pub fn start(
            program: &str,
            args: &[String],
            server_name: &str,
            root_uri: &str,
            timeout: Duration,
            profile: Option<&SandboxProfile>,
        ) -> Result<Arc<Self>, PantheonError> {
            let mut cmd = match profile {
                Some(profile) => {
                    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
                    let (cmd, _sandboxed) =
                        build_sandboxed_checked(profile, program, &argv, &cwd_for_root(root_uri))?;
                    cmd
                }
                None => {
                    let mut cmd = Command::new(program);
                    cmd.args(args);
                    cmd
                }
            };
            let mut child = cmd
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| {
                    err(
                        "LSP_SPAWN",
                        format!("spawn {program}: {e}"),
                        "the language server binary is missing or not on PATH",
                    )
                })?;
            let stdout = child.stdout.take().ok_or_else(|| {
                err(
                    "LSP_STDOUT",
                    "child stdout not piped".to_string(),
                    "internal: check LspClient::start wiring",
                )
            })?;
            let stdin = child.stdin.take().ok_or_else(|| {
                err(
                    "LSP_STDIN",
                    "child stdin not piped".to_string(),
                    "internal: check LspClient::start wiring",
                )
            })?;
            let reader_input = stdout;
            // Keep the real child for shutdown(); it is wrapped in a mutex so
            // only `shutdown` may kill it.
            let child = Mutex::new(child);

            let state = Arc::new(Mutex::new(LspState::default()));
            let server_alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let server_name = server_name.to_string();

            // Reader thread: consume LSP frames off stdout, record
            // publishDiagnostics into the cache, and mark the server dead
            // when the stream ends.
            let reader = {
                let state = Arc::clone(&state);
                let alive = Arc::clone(&server_alive);
                let server_name = server_name.clone();
                std::thread::spawn(move || {
                    let mut reader = reader_input;
                    let mut buf = Vec::new();
                    loop {
                        match read_frame(&mut reader, &mut buf) {
                            Ok(Some(msg)) => {
                                if let Some(diag) = flatten_publish_diagnostics(&msg, &server_name)
                                {
                                    let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
                                    guard.diagnostics.insert(diag.uri.clone(), diag);
                                } else if msg.get("id").is_some() {
                                    // A JSON-RPC response (not a notification):
                                    // park it so `request` can pick it up.
                                    let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
                                    if let Some(err_val) = msg.get("error") {
                                        guard.pending_response = Some(Err(err_val.to_string()));
                                    } else {
                                        guard.pending_response = Some(Ok(msg
                                            .get("result")
                                            .cloned()
                                            .unwrap_or(Value::Null)));
                                    }
                                }
                            }
                            Ok(None) => break, // clean EOF: server exited
                            Err(_) => break,   // read error: treat as dead
                        }
                    }
                    alive.store(false, std::sync::atomic::Ordering::Release);
                })
            };

            let client = LspClient {
                child,
                stdin: Mutex::new(stdin),
                next_id: Mutex::new(1),
                state,
                server: server_name.clone(),
                reader: Mutex::new(Some(reader)),
                server_alive,
            };
            // The handshake writes are done through client.send_request.
            client.initialize(root_uri, &server_name, timeout)?;
            Ok(Arc::new(client))
        }

        fn send(&self, msg: &Value) -> Result<(), PantheonError> {
            let header = format!("Content-Length: {}\r\n\r\n", msg.to_string().len());
            let mut guard = self.stdin.lock().unwrap_or_else(|p| p.into_inner());
            guard
                .write_all(header.as_bytes())
                .and_then(|_| guard.write_all(msg.to_string().as_bytes()))
                .and_then(|_| guard.flush())
                .map_err(|e| {
                    err(
                        "LSP_WRITE",
                        format!("write to server: {e}"),
                        "the language server connection is broken",
                    )
                })
        }

        /// Fire a JSON-RPC request and wait for its response or a timeout.
        fn request(
            &self,
            method: &str,
            params: Value,
            timeout: Duration,
        ) -> Result<Value, PantheonError> {
            if !self.server_alive.load(std::sync::atomic::Ordering::Acquire) {
                return Err(err(
                    "LSP_DEAD",
                    format!("{} server has exited", self.server),
                    "the language server stopped; start a fresh one",
                ));
            }
            let id = {
                let mut g = self.next_id.lock().unwrap_or_else(|p| p.into_inner());
                let n = *g;
                *g += 1;
                n
            };
            let msg = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            });
            self.send(&msg)?;

            // Wait for the reader thread to park a response for this request.
            let deadline = std::time::Instant::now() + timeout;
            loop {
                if !self.server_alive.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(err(
                        "LSP_DEAD",
                        format!("{} exited while waiting for {method}", self.server),
                        "the language server stopped during the handshake; check its stderr",
                    ));
                }
                // Check whether the reader parked a response for us.
                let maybe = {
                    let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    guard.pending_response.take()
                };
                if let Some(res) = maybe {
                    return res.map_err(|e| {
                        err(
                            "LSP_RPC_ERROR",
                            format!("server returned an error for {method}: {e}"),
                            "the language server rejected the request; inspect its stderr",
                        )
                    });
                }
                if std::time::Instant::now() >= deadline {
                    return Err(err(
                        "LSP_TIMEOUT",
                        format!("no response to {method} within {timeout:?}"),
                        "the language server is hung; kill it and restart",
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }

        /// Run the initialize handshake.
        fn initialize(
            &self,
            root_uri: &str,
            _server_name: &str,
            timeout: Duration,
        ) -> Result<(), PantheonError> {
            let params = serde_json::json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": {},
                "workspaceFolders": [{ "uri": root_uri, "name": "workspace" }],
            });
            self.request("initialize", params, timeout)?;
            // `initialized` is a notification (no id, no response).
            let note = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "initialized",
                "params": null,
            });
            self.send(&note)?;
            Ok(())
        }

        /// Open a document with full text; the server will emit diagnostics
        /// via `publishDiagnostics`, which the reader caches.
        pub fn open_document(
            &self,
            path: &Path,
            language_id: &str,
            text: &str,
        ) -> Result<(), PantheonError> {
            let uri = path_uri(path);
            let msg = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {
                    "textDocument": {
                        "uri": uri,
                        "languageId": language_id,
                        "version": 1,
                        "text": text,
                    }
                }
            });
            self.send(&msg)?;
            Ok(())
        }

        /// Wait up to `timeout` for the reader thread to have populated
        /// *meaningful* diagnostics for the given file.
        ///
        /// A real LSP server (e.g. rust-analyzer) emits an empty
        /// `publishDiagnostics` (count 0) the instant `textDocument/didOpen`
        /// is processed, *before* the project model and type-check finish; the
        /// actual errors arrive in a later batch. Returning on the first batch
        /// would therefore hand the caller the premature empty one. So this
        /// keeps polling until a batch with `count > 0` appears, or the
        /// deadline passes.
        ///
        /// Returns `Some` of the latest batch when one (empty or not) was seen
        /// by the deadline, and `None` when the server emitted nothing for this
        /// file at all. The caller is responsible for interpreting an empty
        /// `Some` as "no diagnostics yet - the server may still be analyzing".
        pub fn wait_diagnostics(
            &self,
            uri: &str,
            timeout: Duration,
        ) -> Result<Option<Diagnostics>, PantheonError> {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                let got = {
                    let g = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    g.diagnostics.get(uri).cloned()
                };
                // A non-empty batch is the definitive "analysis produced results"
                // signal - stop here.
                if got.as_ref().is_some_and(|d| d.count > 0) {
                    return Ok(got);
                }
                if std::time::Instant::now() >= deadline {
                    // Deadline passed. Hand back whatever (possibly empty)
                    // batch was last seen, or None if the server never spoke
                    // for this file at all.
                    return Ok(got);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        /// Read the current diagnostics cache for one file.
        pub fn diagnostics(&self, uri: &str) -> Option<Diagnostics> {
            let g = self.state.lock().unwrap_or_else(|p| p.into_inner());
            g.diagnostics.get(uri).cloned()
        }

        /// Read the whole diagnostics cache (all files).
        pub fn all_diagnostics(&self) -> Vec<Diagnostics> {
            let g = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let mut v: Vec<_> = g.diagnostics.values().cloned().collect();
            v.sort_by_key(|d| std::cmp::Reverse(d.updated_ms));
            v
        }

        /// Shut the server down cleanly.
        pub fn shutdown(&self) {
            let _ = self.request("shutdown", Value::Null, Duration::from_secs(3));
            let _ = self.send(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "exit"
            }));
            if let Ok(mut guard) = self.child.try_lock() {
                let _ = guard.kill();
            }
            self.server_alive
                .store(false, std::sync::atomic::Ordering::Release);
            // `reader` is a Mutex<Option<JoinHandle>> so `shutdown` can take it
            // out through `&self` (it is reachable through an `Arc`).
            if let Some(h) = self.reader.lock().unwrap_or_else(|p| p.into_inner()).take() {
                let _ = h.join();
            }
        }
    }

    /// Read one LSP frame: parse a `Content-Length` header, then that many
    /// bytes of UTF-8 JSON. Returns None on clean EOF.
    fn read_frame(r: &mut ChildStdout, buf: &mut Vec<u8>) -> Result<Option<Value>, std::io::Error> {
        // Accumulate raw header bytes until the `\r\n\r\n` terminator. LSP
        // headers always end in `\r\n\r\n`; a lone `\n` stream is not valid
        // LSP, so we key on the CRLF terminator.
        let mut header_bytes: Vec<u8> = Vec::new();
        let mut one = [0u8; 1];
        loop {
            match r.read(&mut one) {
                Ok(0) => return Ok(None),
                Ok(_) => {
                    header_bytes.push(one[0]);
                    if header_bytes.len() >= 4 && header_bytes.windows(4).any(|w| w == b"\r\n\r\n")
                    {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        let header = String::from_utf8_lossy(&header_bytes).to_string();
        let len = header
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .ok_or_else(|| std::io::Error::other("bad Content-Length"))?;
        buf.clear();
        buf.resize(len, 0);
        r.read_exact(buf)?;
        let v: Value = serde_json::from_slice(buf)?;
        Ok(Some(v))
    }

    /// Pull `publishDiagnostics` out of a raw JSON-RPC message and flatten it
    /// into cache form. Returns None for any other message type.
    ///
    /// The real LSP notification is `textDocument/publishDiagnostics` (with the
    /// `textDocument/` namespace prefix); some server implementations and our
    /// own mock historically emitted the short `publishDiagnostics` form.
    /// Accept both so a client change never silently breaks against a real
    /// server again - this exact mismatch is why the tool returned empty
    /// diagnostics for real rust-analyzer while the mock test stayed green.
    fn flatten_publish_diagnostics(msg: &Value, server: &str) -> Option<Diagnostics> {
        let method = msg.get("method")?.as_str()?;
        if method != "textDocument/publishDiagnostics" && method != "publishDiagnostics" {
            return None;
        }
        let params = msg.get("params")?;
        let uri = params.get("uri")?.as_str()?.to_string();
        let raw = params.get("diagnostics")?.as_array()?;
        let mut lines = Vec::new();
        for d in raw {
            let sev = d.get("severity").and_then(|s| s.as_u64()).unwrap_or(1);
            let sev_s = match sev {
                1 => "error",
                2 => "warning",
                3 => "info",
                _ => "hint",
            };
            let line = d
                .pointer("/range/start/line")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let col = d
                .pointer("/range/start/character")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let msg = d.get("message").and_then(|m| m.as_str()).unwrap_or("");
            lines.push(format!("[{sev_s}] {line}:{col} {msg}"));
        }
        Some(Diagnostics {
            uri,
            server: server.to_string(),
            count: lines.len(),
            lines,
            updated_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        })
    }

    /// Walk up from a file to find its project root, i.e. the nearest
    /// directory containing a recognised project manifest. This is what a
    /// language server needs as its `rootUri`: rust-analyzer only fully
    /// analyzes a file when the file belongs to the workspace it was
    /// initialized for. Without it, `lsp.open` on a file in a *different*
    /// project silently returns empty diagnostics because the server is
    /// rooted at the wrong place.
    ///
    /// The probe set is broad enough to cover the languages this harness
    /// targets. The walk stops at the filesystem root and returns the starting
    /// directory itself when no manifest is found (a file with no project is
    /// still a valid single-file analysis target).
    pub fn resolve_workspace_root(path: &Path) -> PathBuf {
        const MARKERS: &[&str] = &[
            "Cargo.toml",
            "package.json",
            "pyproject.toml",
            "go.mod",
            "pom.xml",
            "build.gradle",
            "build.gradle.kts",
            "CMakeLists.txt",
            ".git",
        ];
        let mut dir = match path.parent() {
            Some(d) => d.to_path_buf(),
            None => PathBuf::from(path),
        };
        loop {
            for m in MARKERS {
                if dir.join(m).exists() {
                    return dir;
                }
            }
            match dir.parent() {
                Some(p) => dir = p.to_path_buf(),
                None => return dir,
            }
        }
    }

    /// Convert a filesystem path to a `file://` URI.
    pub fn path_uri(path: &Path) -> String {
        let uri = PathBuf::from(path).to_string_lossy().replace('\\', "/");
        format!("file://{uri}")
    }

    /// Language -> LSP server command. Only the common ones; everything else
    /// falls back to `None` and the caller reports "no server for this
    /// language".
    pub fn server_for(language: &str) -> Option<(String, Vec<String>)> {
        match language.to_lowercase().as_str() {
            "rust" => Some((
                "rust-analyzer".into(),
                // rust-analyzer gets its project root from the LSP initialize
                // handshake's rootUri, not from a CLI flag. Newer releases
                // (1.80+) reject an unknown --path and exit immediately, which
                // surfaces as LSP_DEAD at handshake. No args: the server speaks
                // plain LSP on stdin/stdout and discovers the workspace from
                // the didOpen/initialize payload.
                Vec::new(),
            )),
            "python" => Some((
                "python3".into(),
                vec!["-m".into(), "python-lsp-server".into()],
            )),
            "typescript" | "javascript" => Some((
                "node".into(),
                vec![
                    "node_modules/typescript-language-server/lib/cli.mjs".into(),
                    "--stdio".into(),
                ],
            )),
            "go" => Some((
                "gopls".into(),
                vec!["-rpc".into(), "-mode".into(), "stdio".into()],
            )),
            "cpp" | "c" => Some((
                "clangd".into(),
                vec![
                    "--background-index".into(),
                    "--completion-use-diagnostics".into(),
                ],
            )),
            _ => None,
        }
    }

    /// Detect a language from a file extension.
    pub fn language_for(ext: &str) -> Option<&'static str> {
        match ext.to_lowercase().as_str() {
            "rs" => Some("rust"),
            "py" => Some("python"),
            "ts" | "tsx" => Some("typescript"),
            "js" | "jsx" | "mjs" => Some("javascript"),
            "go" => Some("go"),
            "c" => Some("c"),
            "cpp" | "cc" | "cxx" | "h" | "hpp" => Some("cpp"),
            _ => None,
        }
    }
}
pub mod codegraph {
    //! A small, native Rust codebase graph.
    //!
    //! `syn` parses each `.rs` file under a root; the visitor records symbols
    //! (functions, methods, structs, enums, traits, modules) and call edges
    //! (`foo()` / `x.foo()`). The result is a cacheable JSON graph with
    //! search, caller, callee, and overview queries - enough for an agent to
    //! orient in a repository without a language server or an external MCP
    //! index.
    //!
    //! Deliberately static and lexical: calls are resolved by the final path
    //! segment, so overloads and trait dispatch can produce several matches.
    //! That is useful as a "who mentions/calls this name" index, not as a
    //! compiler-grade reference graph. `rust-analyzer` remains the authority
    //! for type-aware navigation; this fills the gap when it is unavailable.

    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::logging::now_ms;
    use serde::{Deserialize, Serialize};
    use std::path::{Path, PathBuf};
    use std::time::UNIX_EPOCH;
    use syn::spanned::Spanned;
    use syn::visit::{self, Visit};

    fn err(code: &str, cause: String) -> PantheonError {
        PantheonError::new(
            code,
            Layer::Execution,
            false,
            cause,
            "check the repository path and try again",
            "",
        )
    }

    /// One symbol in the graph.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct CodeNode {
        pub id: String,
        pub name: String,
        pub qualified: String,
        /// `function`, `method`, `struct`, `enum`, `trait`, `module`, `type`.
        pub kind: String,
        pub file: String,
        pub line: usize,
    }

    /// One call edge. `to_name` is the lexical callee name; resolution to a
    /// concrete node is done at query time so ambiguous names are not lost.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct CodeEdge {
        pub from: String,
        pub from_name: String,
        pub to_name: String,
        pub file: String,
        pub line: usize,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct CodeGraph {
        pub root: String,
        pub generated_ms: u64,
        /// Fingerprint of indexed Rust file paths, sizes, and modification times.
        /// Empty on older cached indexes, which forces one rebuild after upgrade.
        #[serde(default)]
        pub source_fingerprint: String,
        pub files: usize,
        pub parse_errors: usize,
        pub nodes: Vec<CodeNode>,
        pub edges: Vec<CodeEdge>,
    }

    #[derive(Debug, Clone, Serialize)]
    pub struct CallHit {
        pub node: CodeNode,
        pub edge: CodeEdge,
    }

    #[derive(Debug, Clone, Serialize)]
    pub struct Overview {
        pub root: String,
        pub files: usize,
        pub parse_errors: usize,
        pub symbols: usize,
        pub calls: usize,
        pub by_kind: std::collections::BTreeMap<String, usize>,
        pub top_directories: Vec<(String, usize)>,
    }

    impl CodeGraph {
        /// Parse every Rust file under `root` (canonicalized). Unreadable or
        /// unparseable files are counted, not fatal: a graph over a partially
        /// generated tree is still useful.
        pub fn build(root: impl AsRef<Path>) -> Result<Self, PantheonError> {
            let root = std::fs::canonicalize(root.as_ref()).map_err(|e| {
                err(
                    "CODEGRAPH_ROOT",
                    format!("canonicalize {}: {e}", root.as_ref().display()),
                )
            })?;
            let mut files = Vec::new();
            walk(&root, &mut files, 0)?;
            files.sort();
            let source_fingerprint = fingerprint_files(&root, &files);
            let mut graph = Self {
                root: root.to_string_lossy().to_string(),
                generated_ms: 0,
                source_fingerprint,
                files: files.len(),
                parse_errors: 0,
                nodes: Vec::new(),
                edges: Vec::new(),
            };
            for file in files {
                let rel = file
                    .strip_prefix(&root)
                    .unwrap_or(&file)
                    .to_string_lossy()
                    .replace('\\', "/");
                let src = match std::fs::read_to_string(&file) {
                    Ok(s) => s,
                    Err(_) => {
                        graph.parse_errors += 1;
                        continue;
                    }
                };
                let ast = match syn::parse_file(&src) {
                    Ok(a) => a,
                    Err(_) => {
                        graph.parse_errors += 1;
                        continue;
                    }
                };
                let mut c = Collector::new(rel);
                c.visit_file(&ast);
                graph.nodes.extend(c.nodes);
                graph.edges.extend(c.edges);
            }
            // Stable, duplicate-free output. Two identical call edges in one
            // function (e.g. two `log()` calls) collapse to one lexical edge;
            // line numbers keep distinct call sites distinct.
            graph.nodes.sort_by(|a, b| {
                a.file
                    .cmp(&b.file)
                    .then(a.line.cmp(&b.line))
                    .then(a.id.cmp(&b.id))
            });
            graph.edges.sort_by(|a, b| {
                a.file
                    .cmp(&b.file)
                    .then(a.line.cmp(&b.line))
                    .then(a.from.cmp(&b.from))
                    .then(a.to_name.cmp(&b.to_name))
            });
            graph.edges.dedup();
            graph.generated_ms = now_ms() as u64;
            Ok(graph)
        }

        /// Check whether this index still describes the current Rust source
        /// tree. The metadata fingerprint catches normal edits; the five-minute
        /// age bound also catches edits that preserve size and modification time.
        pub fn is_current_for(&self, root: impl AsRef<Path>) -> bool {
            let root = match std::fs::canonicalize(root.as_ref()) {
                Ok(root) => root,
                Err(_) => return false,
            };
            if Path::new(&self.root) != root || self.source_fingerprint.is_empty() {
                return false;
            }
            let age_ms = (now_ms() as u64).saturating_sub(self.generated_ms);
            if age_ms > 300_000 {
                return false;
            }
            let mut files = Vec::new();
            if walk(&root, &mut files, 0).is_err() {
                return false;
            }
            files.sort();
            fingerprint_files(&root, &files) == self.source_fingerprint
        }

        /// Legacy single-index location. Prefer [`Self::cache_path_for`] when
        /// the repository root is known: one data dir may serve several
        /// checkouts (main tree + worktrees), and a single file would thrash.
        pub fn cache_path(state_dir: &Path) -> PathBuf {
            state_dir.join("codegraph").join("index.json")
        }

        /// Per-repository cache path: `<state>/codegraph/<root-hash>.json`.
        /// The root is canonicalized first so `foo` and `/abs/foo` share one
        /// entry.
        pub fn cache_path_for(state_dir: &Path, root: &Path) -> PathBuf {
            let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
            let hash =
                crate::sandbox_group::safewrite::fnv1a_hex(canonical.to_string_lossy().as_bytes());
            state_dir
                .join("codegraph")
                .join(format!("{}.json", &hash[..16]))
        }

        pub fn save(&self, path: &Path) -> Result<(), PantheonError> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    err(
                        "CODEGRAPH_MKDIR",
                        format!("create {}: {e}", parent.display()),
                    )
                })?;
            }
            let raw = serde_json::to_string(self)
                .map_err(|e| err("CODEGRAPH_JSON", format!("serialize index: {e}")))?;
            let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
            std::fs::write(&tmp, raw)
                .and_then(|_| std::fs::rename(&tmp, path))
                .map_err(|e| err("CODEGRAPH_WRITE", format!("write {}: {e}", path.display())))
        }

        pub fn load(path: &Path) -> Option<Self> {
            let raw = std::fs::read_to_string(path).ok()?;
            serde_json::from_str(&raw).ok()
        }

        pub fn search(&self, query: &str, limit: usize) -> Vec<&CodeNode> {
            let q = query.to_ascii_lowercase();
            self.nodes
                .iter()
                .filter(|n| {
                    n.name.to_ascii_lowercase().contains(&q)
                        || n.qualified.to_ascii_lowercase().contains(&q)
                        || n.file.to_ascii_lowercase().contains(&q)
                })
                .take(limit)
                .collect()
        }

        /// Call sites whose lexical callee matches `symbol` (exact final
        /// segment, case-sensitive). Returns the caller node plus the edge.
        pub fn callers(&self, symbol: &str, limit: usize) -> Vec<CallHit> {
            let by_id: std::collections::HashMap<&str, &CodeNode> =
                self.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
            self.edges
                .iter()
                .filter(|e| e.to_name == symbol)
                .filter_map(|e| {
                    by_id.get(e.from.as_str()).map(|n| CallHit {
                        node: (*n).clone(),
                        edge: e.clone(),
                    })
                })
                .take(limit)
                .collect()
        }

        /// Outgoing lexical call edges from nodes named `symbol`. Each edge
        /// carries `to_name` (the callee's final path segment) because a
        /// lexical graph cannot resolve overloads or trait dispatch.
        pub fn callees(&self, symbol: &str, limit: usize) -> Vec<CodeEdge> {
            let from: std::collections::HashSet<&str> = self
                .nodes
                .iter()
                .filter(|n| n.name == symbol || n.qualified.ends_with(&format!("::{symbol}")))
                .map(|n| n.id.as_str())
                .collect();
            self.edges
                .iter()
                .filter(|e| from.contains(e.from.as_str()))
                .take(limit)
                .cloned()
                .collect()
        }

        pub fn overview(&self) -> Overview {
            let mut by_kind = std::collections::BTreeMap::new();
            for n in &self.nodes {
                *by_kind.entry(n.kind.clone()).or_insert(0) += 1;
            }
            let mut dirs: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            for n in &self.nodes {
                let dir = n
                    .file
                    .rsplit_once('/')
                    .map(|(d, _)| d.to_string())
                    .unwrap_or_else(|| ".".to_string());
                *dirs.entry(dir).or_insert(0) += 1;
            }
            let mut top_directories: Vec<(String, usize)> = dirs.into_iter().collect();
            top_directories.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            top_directories.truncate(20);
            Overview {
                root: self.root.clone(),
                files: self.files,
                parse_errors: self.parse_errors,
                symbols: self.nodes.len(),
                calls: self.edges.len(),
                by_kind,
                top_directories,
            }
        }
    }

    fn fingerprint_files(root: &Path, files: &[PathBuf]) -> String {
        let mut bytes = Vec::new();
        for file in files {
            let rel = file.strip_prefix(root).unwrap_or(file).to_string_lossy();
            bytes.extend_from_slice(rel.as_bytes());
            bytes.push(0);
            match std::fs::metadata(file) {
                Ok(meta) => {
                    bytes.extend_from_slice(&meta.len().to_le_bytes());
                    let modified_ns = meta
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                        .map(|duration| duration.as_nanos())
                        .unwrap_or_default();
                    bytes.extend_from_slice(&modified_ns.to_le_bytes());
                }
                Err(_) => {
                    // An unreadable or disappearing file must not look like a
                    // valid cached source entry.
                    bytes.extend_from_slice(&u64::MAX.to_le_bytes());
                    bytes.extend_from_slice(&u128::MAX.to_le_bytes());
                }
            }
            bytes.push(0xff);
        }
        let mut hash: u64 = 0xcbf29ce484222325;
        for byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        format!("{hash:016x}")
    }

    fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) -> Result<(), PantheonError> {
        if depth > 32 || out.len() >= 20_000 {
            return Ok(());
        }
        let entries = std::fs::read_dir(dir)
            .map_err(|e| err("CODEGRAPH_READ", format!("read {}: {e}", dir.display())))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if matches!(
                    name.as_str(),
                    ".git"
                        | "target"
                        | "node_modules"
                        | "vendor"
                        | "dist"
                        | "build"
                        | ".venv"
                        | "venv"
                        | "__pycache__"
                ) {
                    continue;
                }
                walk(&path, out, depth + 1)?;
            } else if file_type.is_file()
                && path.extension().and_then(|e| e.to_str()) == Some("rs")
                && std::fs::metadata(&path)
                    .map(|m| m.len() <= 2 * 1024 * 1024)
                    .unwrap_or(false)
            {
                out.push(path);
            }
        }
        Ok(())
    }

    struct Collector {
        file: String,
        module: Vec<String>,
        impl_type: Option<String>,
        current_fn: Option<(String, String)>,
        nodes: Vec<CodeNode>,
        edges: Vec<CodeEdge>,
        seq: usize,
    }

    impl Collector {
        fn new(file: String) -> Self {
            let module = module_from_file(&file);
            Self {
                file,
                module,
                impl_type: None,
                current_fn: None,
                nodes: Vec::new(),
                edges: Vec::new(),
                seq: 0,
            }
        }

        fn qualified(&self, name: &str) -> String {
            let mut parts = self.module.clone();
            if let Some(t) = &self.impl_type {
                parts.push(t.clone());
            }
            parts.push(name.to_string());
            parts.join("::")
        }

        fn add_node(&mut self, name: &str, kind: &str, line: usize) {
            self.seq += 1;
            let qualified = self.qualified(name);
            self.nodes.push(CodeNode {
                id: format!("{}:{}:{}:{}", self.file, line, self.seq, qualified),
                name: name.to_string(),
                qualified,
                kind: kind.to_string(),
                file: self.file.clone(),
                line,
            });
        }

        fn add_edge(&mut self, to: &str, line: usize) {
            let Some((from, from_name)) = self.current_fn.clone() else {
                return;
            };
            self.edges.push(CodeEdge {
                from,
                from_name,
                to_name: to.to_string(),
                file: self.file.clone(),
                line,
            });
        }
    }

    fn module_from_file(file: &str) -> Vec<String> {
        let mut parts: Vec<String> = file
            .trim_end_matches(".rs")
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if let Some(pos) = parts.iter().position(|p| p == "src") {
            parts.drain(..=pos);
        }
        if matches!(
            parts.last().map(String::as_str),
            Some("lib") | Some("main") | Some("mod")
        ) {
            parts.pop();
        }
        parts
    }

    impl<'ast> Visit<'ast> for Collector {
        fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
            let name = i.ident.to_string();
            self.add_node(&name, "module", i.span().start().line);
            self.module.push(name);
            visit::visit_item_mod(self, i);
            self.module.pop();
        }

        fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
            let name = i.sig.ident.to_string();
            let line = i.span().start().line;
            self.add_node(&name, "function", line);
            let id = self.nodes.last().map(|n| n.id.clone()).unwrap_or_default();
            let prev = self.current_fn.replace((id, name));
            visit::visit_item_fn(self, i);
            self.current_fn = prev;
        }

        fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
            self.add_node(&i.ident.to_string(), "struct", i.span().start().line);
            visit::visit_item_struct(self, i);
        }

        fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
            self.add_node(&i.ident.to_string(), "enum", i.span().start().line);
            visit::visit_item_enum(self, i);
        }

        fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
            let name = i.ident.to_string();
            self.add_node(&name, "trait", i.span().start().line);
            let prev = self.impl_type.replace(name);
            visit::visit_item_trait(self, i);
            self.impl_type = prev;
        }

        fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
            self.add_node(&i.ident.to_string(), "type", i.span().start().line);
            visit::visit_item_type(self, i);
        }

        fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
            let prev = self.impl_type.clone();
            if let Some(ty) = type_name(&i.self_ty) {
                self.impl_type = Some(ty);
            }
            visit::visit_item_impl(self, i);
            self.impl_type = prev;
        }

        fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
            let name = i.sig.ident.to_string();
            self.add_node(&name, "method", i.span().start().line);
            let id = self.nodes.last().map(|n| n.id.clone()).unwrap_or_default();
            let prev = self.current_fn.replace((id, name));
            visit::visit_impl_item_fn(self, i);
            self.current_fn = prev;
        }

        fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
            let name = i.sig.ident.to_string();
            self.add_node(&name, "method", i.span().start().line);
            let id = self.nodes.last().map(|n| n.id.clone()).unwrap_or_default();
            let prev = self.current_fn.replace((id, name));
            visit::visit_trait_item_fn(self, i);
            self.current_fn = prev;
        }

        fn visit_expr_call(&mut self, i: &'ast syn::ExprCall) {
            if let Some(name) = callee_name(&i.func) {
                self.add_edge(&name, i.span().start().line);
            }
            visit::visit_expr_call(self, i);
        }

        fn visit_expr_method_call(&mut self, i: &'ast syn::ExprMethodCall) {
            self.add_edge(&i.method.to_string(), i.span().start().line);
            visit::visit_expr_method_call(self, i);
        }
    }

    fn callee_name(e: &syn::Expr) -> Option<String> {
        match e {
            syn::Expr::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
            syn::Expr::Paren(p) => callee_name(&p.expr),
            syn::Expr::Group(g) => callee_name(&g.expr),
            _ => None,
        }
    }

    fn type_name(ty: &syn::Type) -> Option<String> {
        let syn::Type::Path(p) = ty else {
            return None;
        };
        p.path.segments.last().map(|s| s.ident.to_string())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn write(root: &Path, rel: &str, body: &str) {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }

        #[test]
        fn indexes_symbols_and_calls() {
            let tmp = tempfile::tempdir().unwrap();
            write(
                tmp.path(),
                "src/lib.rs",
                r#"
pub fn run() {
    helper();
    thing.go();
}

fn helper() {}

struct Thing;
impl Thing {
    fn go(&self) {}
}
"#,
            );
            let g = CodeGraph::build(tmp.path()).unwrap();
            assert_eq!(g.files, 1);
            assert_eq!(g.parse_errors, 0);
            assert!(g
                .nodes
                .iter()
                .any(|n| n.name == "run" && n.kind == "function"));
            assert!(g.nodes.iter().any(|n| n.name == "go" && n.kind == "method"));
            assert!(g
                .nodes
                .iter()
                .any(|n| n.name == "Thing" && n.kind == "struct"));
            let hits = g.callers("helper", 10);
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].node.name, "run");
            let hits = g.callers("go", 10);
            assert_eq!(hits.len(), 1);
            let hits = g.callees("run", 10);
            assert_eq!(hits.len(), 2);
            assert!(g.overview().symbols >= 4);
        }

        #[test]
        fn skips_target_and_survives_parse_errors() {
            let tmp = tempfile::tempdir().unwrap();
            write(tmp.path(), "src/lib.rs", "pub fn ok() {}\n");
            write(tmp.path(), "src/broken.rs", "this is not rust {{{\n");
            write(tmp.path(), "target/generated.rs", "pub fn nope() {}\n");
            let g = CodeGraph::build(tmp.path()).unwrap();
            assert_eq!(g.files, 2);
            assert_eq!(g.parse_errors, 1);
            assert!(g.nodes.iter().any(|n| n.name == "ok"));
            assert!(!g.nodes.iter().any(|n| n.name == "nope"));
            assert!(g.search("ok", 10).iter().any(|n| n.name == "ok"));
        }

        #[test]
        fn cache_round_trips() {
            let tmp = tempfile::tempdir().unwrap();
            write(tmp.path(), "src/lib.rs", "pub fn cached() {}\n");
            let g = CodeGraph::build(tmp.path()).unwrap();
            let path = CodeGraph::cache_path(tmp.path());
            g.save(&path).unwrap();
            let loaded = CodeGraph::load(&path).unwrap();
            assert_eq!(loaded.nodes.len(), g.nodes.len());
            assert!(loaded.search("cached", 10).len() == 1);
        }

        #[test]
        fn per_repository_cache_paths_do_not_collide() {
            let a = tempfile::tempdir().unwrap();
            let b = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            let pa = CodeGraph::cache_path_for(state.path(), a.path());
            let pb = CodeGraph::cache_path_for(state.path(), b.path());
            assert_ne!(pa, pb);
            assert_eq!(pa, CodeGraph::cache_path_for(state.path(), a.path()));
            assert_eq!(
                pa.parent().and_then(|p| p.file_name()),
                Some(std::ffi::OsStr::new("codegraph"))
            );
        }
    }
}
pub mod gitundo {
    //! Git-based undo: repo-level snapshots for batch workspaces.
    //!
    //! `safewrite` checkpoints individual file edits. This module snapshots
    //! the entire git working tree before a batch of tool calls, creating
    //! a lightweight undo point that can restore all changed files at once.
    //!
    //! Model:
    //! - A snapshot commits the *current* working tree to a dedicated ref
    //!   `refs/pantheon/undo/<id>` (so it never touches the user's branch
    //!   history) and stores the diff between that snapshot and the working
    //!   tree as a patch. The patch is what gets reversed on restore.
    //! - `restore` reverses the stored patch (`git apply -R`), which moves
    //!   the working tree back to the state the snapshot captured. If the
    //!   reverse-apply is impossible (files have diverged), it falls back to
    //!   checking the snapshot commit's tree straight out over the working dir.
    //!
    //! This complements `safewrite`: use it when a task touches many files
    //! and you want "undo everything since the last checkpoint" without
    //! replaying per-file operations.
    use pantheon_api::error::{Layer, PantheonError};
    use serde::{Deserialize, Serialize};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Git undo ref namespace, isolated from user refs.
    pub const UNDO_REF_PREFIX: &str = "refs/pantheon/undo/";

    /// One undo point: a snapshot of the working tree taken before a batch.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct UndoPoint {
        /// Unique id, e.g. `undo_1730891234567_0001`.
        pub id: String,
        /// Unix ms when the snapshot was taken.
        pub created_ms: i64,
        /// Head commit when the snapshot was taken (for reference).
        pub head_sha: String,
        /// The undo ref this snapshot lives on.
        pub ref_name: String,
        /// Relative path to the stored patch file (inside the state dir).
        pub patch_path: String,
        /// Files that differ between the snapshot and HEAD (for display).
        pub changed_files: Vec<String>,
        /// FNV-1a hash of the patch bytes for integrity.
        pub patch_hash: String,
    }

    /// Result of a successful undo restore.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct UndoRestore {
        /// The undo point id that was restored.
        pub undo_id: String,
        /// How many files were affected by the snapshot.
        pub restored_files: usize,
        /// True when the patch reversed cleanly; false when the git-checkout
        /// fallback was used instead.
        pub via_patch: bool,
    }

    /// Manager for git-based undo points. All state lives under `state_dir`
    /// and a target git repo.
    #[derive(Debug, Clone)]
    pub struct GitUndo {
        state_dir: PathBuf,
        repo_dir: PathBuf,
    }

    fn err(code: &str, cause: String, remediation: &str) -> PantheonError {
        PantheonError::new(code, Layer::Execution, false, cause, remediation, "")
    }

    impl GitUndo {
        pub fn new(state_dir: PathBuf, repo_dir: PathBuf) -> Result<Self, PantheonError> {
            let dir = state_dir.join("git_undo");
            std::fs::create_dir_all(&dir).map_err(|e| {
                err(
                    "UNDO_MKDIR",
                    format!("create git undo dir {}: {e}", dir.display()),
                    "check state dir permissions and disk space",
                )
            })?;
            Ok(Self {
                state_dir: dir,
                repo_dir,
            })
        }

        /// Take a snapshot of the current working tree.
        ///
        /// Commits the full working tree (including untracked files) to a side
        /// commit, points `refs/pantheon/undo/<id>` at it, and stores the
        /// HEAD-to-snapshot diff as a patch. The user's branch is never moved.
        pub fn snapshot(&self, label: &str) -> Result<UndoPoint, PantheonError> {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let id = format!(
                "undo_{now}_{:04}",
                std::process::id().wrapping_add((now % 10000) as u32) % 10000
            );
            let ref_name = format!("{UNDO_REF_PREFIX}{id}");
            let head_sha = self.git("rev-parse", &["HEAD"])?;

            // Capture the current tree (tracked + untracked) into the index,
            // make a side commit, point the undo ref at it, then restore the
            // index so the working tree looks exactly as it did before.
            let _ = self.git_raw("add", &["-A", "--", "."]);
            let commit = self.git_raw(
                "commit",
                &[
                    "--allow-empty",
                    "-m",
                    &format!("pantheon undo {label}"),
                    "--quiet",
                ],
            );
            if !commit.success {
                return Err(err(
                    "UNDO_COMMIT",
                    format!("git commit failed: {}", commit.stderr),
                    "inspect the repo state; the index may be empty or broken",
                ));
            }
            let snap_sha = self.git("rev-parse", &["HEAD"])?;
            let _ = self.git_raw("update-ref", &[ref_name.as_str(), &snap_sha]);
            // Undo the side commit without touching the working tree.
            let _ = self.git_raw("reset", &["--mixed", head_sha.as_str()]);

            // Store the patch that moves the working tree from its current
            // state back to the snapshot state: a forward diff from the base
            // commit to the snapshot commit. On restore, `git apply` moves the
            // tree to match the snapshot: it re-adds deleted files, removes
            // added ones, and reverts modifications.
            let patch_name = format!("{id}.patch");
            let patch_path = self.state_dir.join(&patch_name);
            let diff = self.git("diff", &[head_sha.as_str(), snap_sha.as_str(), "--"])?;
            std::fs::write(&patch_path, diff.as_bytes()).map_err(|e| {
                err(
                    "UNDO_WRITE",
                    format!("write undo patch: {e}"),
                    "check disk space and permissions",
                )
            })?;
            let patch_hash = crate::sandbox_group::safewrite::fnv1a_hex(diff.as_bytes());
            let changed = self.git(
                "diff",
                &["--name-only", head_sha.as_str(), snap_sha.as_str(), "--"],
            )?;
            let changed_files: Vec<String> = changed
                .lines()
                .map(|l| l.to_string())
                .filter(|l| !l.is_empty())
                .collect();

            let point = UndoPoint {
                id: id.clone(),
                created_ms: now,
                head_sha,
                ref_name,
                patch_path: patch_name,
                changed_files,
                patch_hash,
            };
            self.write_manifest(&point)?;
            Ok(point)
        }

        /// Restore the working tree to a previous undo point.
        ///
        /// A stored patch only reverts cleanly when the tree still matches
        /// the base it was diffed against; in general a later batch has
        /// diverged, so the reliable restore is a faithful snapshot check:
        /// unstage everything, check the snapshot's tree out over the
        /// working dir (which reverts modifications and re-adds deleted
        /// files), then prune files that exist now but not in the snapshot.
        ///
        /// The stored patch is still applied first, when it can: it is a
        /// cheaper fast path for the common "one edit, one undo" case and it
        /// leaves the index untouched.
        pub fn restore(&self, point: &UndoPoint) -> Result<UndoRestore, PantheonError> {
            let patch_full = self.state_dir.join(&point.patch_path);
            let raw = std::fs::read(&patch_full).map_err(|e| {
                err(
                    "UNDO_READ",
                    format!("read undo patch {}: {e}", patch_full.display()),
                    "patch file missing or unreadable",
                )
            })?;
            // Integrity: a hash mismatch means the patch was touched; refuse it.
            if crate::sandbox_group::safewrite::fnv1a_hex(&raw) != point.patch_hash {
                return Err(err(
                    "UNDO_CORRUPT",
                    format!(
                        "undo patch {} failed integrity check (hash mismatch)",
                        point.id
                    ),
                    "undo point is corrupt; take a fresh snapshot",
                ));
            }

            // Fast path: the tree is still where the patch was cut, so
            // applying it moves the tree back to the snapshot exactly.
            let apply = self.git_raw(
                "apply",
                &["--ignore-whitespace", patch_full.to_string_lossy().as_ref()],
            );
            if apply.success {
                return Ok(UndoRestore {
                    undo_id: point.id.clone(),
                    restored_files: point.changed_files.len(),
                    via_patch: true,
                });
            }

            // General path: check the snapshot's tree out over the working
            // dir and prune anything not in it. `reset --mixed` first so
            // post-batch additions become untracked (cleanable) rather than
            // staged, then `checkout -f` overwrites tracked content and
            // restores deleted files, then `clean -fd` removes the rest.
            let reset = self.git_raw("reset", &["--mixed", point.head_sha.as_str()]);
            if !reset.success {
                return Err(err(
                    "UNDO_FAIL",
                    format!("undo restore: git reset failed: {}", reset.stderr),
                    "inspect the repo state manually; delete the corrupt undo point",
                ));
            }
            let co = self.git_raw("checkout", &["-f", point.ref_name.as_str(), "--", "."]);
            if !co.success {
                return Err(err(
                    "UNDO_FAIL",
                    format!(
                        "undo restore failed: patch apply and git checkout both failed: {}",
                        co.stderr
                    ),
                    "inspect the repo state manually; delete the corrupt undo point",
                ));
            }
            let _ = self.git_raw("clean", &["-fdq", "--", "."]);
            Ok(UndoRestore {
                undo_id: point.id.clone(),
                restored_files: point.changed_files.len(),
                via_patch: false,
            })
        }

        /// List all stored undo points, newest first.
        pub fn list(&self) -> Result<Vec<UndoPoint>, PantheonError> {
            let mut out = Vec::new();
            let entries = std::fs::read_dir(&self.state_dir).map_err(|e| {
                err(
                    "UNDO_LIST",
                    format!("read undo state dir: {e}"),
                    "state dir missing; create a fresh one",
                )
            })?;
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".json") {
                    if let Ok(raw) = std::fs::read_to_string(entry.path()) {
                        if let Ok(p) = serde_json::from_str::<UndoPoint>(&raw) {
                            out.push(p);
                        }
                    }
                }
            }
            out.sort_by_key(|u| std::cmp::Reverse(u.created_ms));
            Ok(out)
        }

        /// Delete one undo point and its ref.
        pub fn delete(&self, point: &UndoPoint) -> Result<(), PantheonError> {
            let _ = self.git_raw("update-ref", &["-d", point.ref_name.as_str()]);
            let manifest = self.state_dir.join(format!("{}.json", point.id));
            let _ = std::fs::remove_file(&manifest);
            let patch = self.state_dir.join(&point.patch_path);
            let _ = std::fs::remove_file(&patch);
            Ok(())
        }

        // ---- helpers ----

        fn write_manifest(&self, point: &UndoPoint) -> Result<(), PantheonError> {
            let path = self.state_dir.join(format!("{}.json", point.id));
            let raw = serde_json::to_vec(point).map_err(|e| {
                err(
                    "UNDO_SER",
                    format!("serialize undo manifest: {e}"),
                    "retry the snapshot",
                )
            })?;
            std::fs::write(&path, raw).map_err(|e| {
                err(
                    "UNDO_SER",
                    format!("write undo manifest {}: {e}", path.display()),
                    "check disk space",
                )
            })
        }

        /// Run a git subcommand, returning trimmed stdout. Fails hard.
        fn git(&self, head: &str, rest: &[&str]) -> Result<String, PantheonError> {
            let out = Command::new("git")
                .current_dir(&self.repo_dir)
                .arg(head)
                .args(rest)
                .output()
                .map_err(|e| {
                    err(
                        "UNDO_GIT",
                        format!("spawn git: {e}"),
                        "git binary missing or path invalid",
                    )
                })?;
            if !out.status.success() {
                return Err(err(
                    "UNDO_GIT_FAIL",
                    format!(
                        "git {head} failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    ),
                    "inspect the repo; the working tree may be in an unexpected state",
                ));
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }

        /// Run a git subcommand without hard-failing; return success + stderr.
        fn git_raw(&self, head: &str, rest: &[&str]) -> GitResult {
            let Ok(out) = Command::new("git")
                .current_dir(&self.repo_dir)
                .arg(head)
                .args(rest)
                .output()
            else {
                return GitResult {
                    success: false,
                    stderr: "spawn git failed".to_string(),
                };
            };
            GitResult {
                success: out.status.success(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            }
        }
    }

    /// Outcome of a best-effort git subcommand.
    #[derive(Debug, Clone)]
    struct GitResult {
        success: bool,
        stderr: String,
    }

    /// Convenience: a git repo must exist at `repo_dir` for undo to work.
    /// Returns the repo root when one is found, or `None` (undo is a no-op).
    pub fn repo_root(repo_dir: &Path) -> Option<PathBuf> {
        let out = Command::new("git")
            .current_dir(repo_dir)
            .arg("rev-parse")
            .arg("--show-toplevel")
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if root.is_empty() {
            return None;
        }
        Some(PathBuf::from(root))
    }
}
pub mod worktree {
    //! First-class git worktrees: create, list, and remove isolated working
    //! trees without cloning and without disturbing the current checkout.
    //!
    //! A worktree is the natural workspace unit for parallel agent work: each
    //! one is its own `HEAD`, index, and working checkout, while sharing the
    //! repository's object store. Pantheon keeps managed worktrees under
    //! `<state_dir>/worktrees/<repo-name>-<hash>/<name>` so they never nest
    //! inside the repository or collide with another repo's namespace.
    //!
    //! The engine is deliberately thin: every operation shells out to `git
    //! worktree` and parses the porcelain output. Branches created here live
    //! in the `pantheon/<name>` namespace, so `worktree remove` can identify
    //! its own branches and never touches the user's branch naming.

    use pantheon_api::error::{Layer, PantheonError};
    use serde::{Deserialize, Serialize};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use crate::sandbox_group::safewrite::fnv1a_hex;
    use crate::tools_group::gitundo::repo_root;

    fn err(code: &str, cause: String, remediation: &str) -> PantheonError {
        PantheonError::new(code, Layer::Execution, false, cause, remediation, "")
    }

    /// The branch prefix Pantheon uses for worktrees it creates.
    pub const WORKTREE_BRANCH_PREFIX: &str = "pantheon/";

    /// One worktree as reported by `git worktree list --porcelain`.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Worktree {
        pub path: PathBuf,
        pub head: String,
        /// Short branch name (e.g. `main`), when attached to one.
        pub branch: Option<String>,
        pub bare: bool,
        pub detached: bool,
        /// The repository's primary working tree.
        pub main: bool,
        pub locked: bool,
        pub prunable: bool,
    }

    impl Worktree {
        /// The branch Pantheon created for this worktree, if it is one of
        /// ours. Useful for cleanup and for display.
        pub fn pantheon_branch(&self) -> Option<&str> {
            self.branch
                .as_deref()
                .filter(|b| b.starts_with(WORKTREE_BRANCH_PREFIX))
        }
    }

    /// Manage worktrees for one git repository.
    #[derive(Debug, Clone)]
    pub struct WorktreeManager {
        repo: PathBuf,
        root: PathBuf,
    }

    impl WorktreeManager {
        /// Bind to the repository containing `repo_dir` and keep managed
        /// worktrees under `<state_dir>/worktrees/<repo>-<hash>`.
        pub fn new(
            repo_dir: impl AsRef<Path>,
            state_dir: impl AsRef<Path>,
        ) -> Result<Self, PantheonError> {
            let repo = repo_root(repo_dir.as_ref()).ok_or_else(|| {
                err(
                    "WORKTREE_NO_REPO",
                    format!("no git repository found at {}", repo_dir.as_ref().display()),
                    "run the command inside a git working tree or pass a repo path",
                )
            })?;
            // A linked worktree's `--show-toplevel` is that worktree, not the
            // repository's primary tree. Bind the namespace to the primary
            // tree so `pantheon worktree list` run from either location sees
            // the same managed root.
            let main = main_worktree(&repo).unwrap_or_else(|| repo.clone());
            let canonical = std::fs::canonicalize(&main).unwrap_or(main);
            let name = canonical
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "repo".to_string());
            let slug: String = name
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            let hash = fnv1a_hex(canonical.to_string_lossy().as_bytes());
            let namespace = format!("{}-{}", slug.trim_matches('-'), &hash[..8]);
            let mut root = state_dir.as_ref().join("worktrees").join(&namespace);
            // If the data dir happens to be inside the repository (single-dir
            // installs), nesting a worktree inside the checkout it was created
            // from is confusing and can trip recursive tooling. Put managed
            // worktrees beside the repository instead.
            if root.starts_with(&canonical) {
                let parent = canonical.parent().unwrap_or(&canonical);
                root = parent.join(".pantheon-worktrees").join(&namespace);
            }
            Ok(Self {
                repo: canonical,
                root,
            })
        }

        pub fn repo(&self) -> &Path {
            &self.repo
        }

        pub fn root(&self) -> &Path {
            &self.root
        }

        /// Validate a worktree name and return the managed path.
        pub fn path_for(&self, name: &str) -> Result<PathBuf, PantheonError> {
            validate_name(name)?;
            Ok(self.root.join(name))
        }

        /// Find a linked worktree by managed name or by the
        /// `pantheon/<name>` branch. Unlike [`Self::path_for`], this finds a
        /// worktree created with an explicit `--path`.
        pub fn find(&self, name: &str) -> Result<Option<Worktree>, PantheonError> {
            let managed = self.path_for(name)?;
            let branch = format!("{WORKTREE_BRANCH_PREFIX}{name}");
            Ok(self
                .list()?
                .into_iter()
                .find(|w| w.path == managed || w.branch.as_deref() == Some(branch.as_str())))
        }

        /// `git worktree list --porcelain`, parsed. The first entry is the
        /// repository's main working tree.
        pub fn list(&self) -> Result<Vec<Worktree>, PantheonError> {
            let out = self.git(&["worktree", "list", "--porcelain"])?;
            Ok(parse_porcelain(&out))
        }

        /// Create a worktree from `base` (default `HEAD`) on branch
        /// `pantheon/<name>`. A branch that already exists is attached instead
        /// of recreated (useful when retrying after the directory was removed);
        /// if it is checked out somewhere else, git refuses and the error is
        /// returned unchanged. `base` is ignored when the branch already exists.
        ///
        /// `path` overrides the managed location. The caller is responsible
        /// for picking a location outside the repository.
        pub fn create(
            &self,
            name: &str,
            base: Option<&str>,
            path: Option<&Path>,
        ) -> Result<Worktree, PantheonError> {
            validate_name(name)?;
            let managed = self.path_for(name)?;
            let target = match path {
                Some(p) if p.is_absolute() => p.to_path_buf(),
                Some(p) => std::env::current_dir()
                    .map(|cwd| cwd.join(p))
                    .unwrap_or_else(|_| p.to_path_buf()),
                None => managed,
            };
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    err(
                        "WORKTREE_MKDIR",
                        format!("create worktree parent {}: {e}", parent.display()),
                        "check permissions and disk space",
                    )
                })?;
            }
            if target.exists() && !target.is_dir() {
                return Err(err(
                    "WORKTREE_EXISTS",
                    format!("{} exists and is not a directory", target.display()),
                    "choose a different --path or remove the file",
                ));
            }

            let branch = format!("{WORKTREE_BRANCH_PREFIX}{name}");
            let branch_exists = self
                .git_status(&[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}"),
                ])
                .is_some_and(|s| s.status.success());
            let target_s = target.to_string_lossy().to_string();
            let base = base.unwrap_or("HEAD");
            let mut args: Vec<String> = vec!["worktree".into(), "add".into()];
            if !branch_exists {
                args.push("--no-track".into());
                args.push("-b".into());
                args.push(branch.clone());
            }
            args.push(target_s.clone());
            if branch_exists {
                args.push(branch.clone());
            } else {
                args.push(base.to_string());
            }
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            self.git(&refs)?;

            // Return the canonical entry from git, not the path we asked for
            // (git may normalize it).
            let wanted = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
            self.list()?
                .into_iter()
                .find(|w| w.path == wanted || w.path == target)
                .ok_or_else(|| {
                    err(
                        "WORKTREE_NOT_FOUND",
                        format!(
                            "git reported success but {} is not in `worktree list`",
                            target.display()
                        ),
                        "run `git worktree list` to inspect the repository state",
                    )
                })
        }

        /// Remove a Pantheon-managed worktree by name or path. A custom path is
        /// recognized by the `pantheon/` branch namespace. The primary working
        /// tree and unrelated linked worktrees are never removable. The branch
        /// is left in place: deleting it is a separate, deliberate act.
        pub fn remove(&self, target: &str, force: bool) -> Result<Worktree, PantheonError> {
            let path_target = target.contains('/') || target.contains('\\');
            let wanted = if path_target {
                let p = PathBuf::from(target);
                std::fs::canonicalize(&p).unwrap_or(p)
            } else {
                self.path_for(target)?
            };
            let entry = if path_target {
                self.list()?
                    .into_iter()
                    .find(|w| w.path == wanted || w.path == Path::new(target))
            } else {
                self.find(target)?
            }
            .ok_or_else(|| {
                err(
                    "WORKTREE_NOT_FOUND",
                    format!("no worktree at {}", wanted.display()),
                    "run `pantheon worktree list` to see the managed worktrees",
                )
            })?;
            if entry.main {
                return Err(err(
                    "WORKTREE_MAIN",
                    "the repository's primary working tree cannot be removed".to_string(),
                    "remove a linked worktree, or delete the repository manually",
                ));
            }
            let managed_root =
                std::fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
            if !entry.path.starts_with(&managed_root) && entry.pantheon_branch().is_none() {
                return Err(err(
                    "WORKTREE_NOT_MANAGED",
                    format!(
                        "{} is not a Pantheon-managed worktree",
                        entry.path.display()
                    ),
                    "remove it with `git worktree remove`, or use a Pantheon worktree",
                ));
            }
            let path_s = entry.path.to_string_lossy().to_string();
            let mut args = vec!["worktree", "remove"];
            if force {
                args.push("--force");
            }
            args.push(&path_s);
            self.git(&args)?;
            // Prune metadata for any worktree whose directory vanished.
            let _ = self.git(&["worktree", "prune"]);
            Ok(entry)
        }

        fn git(&self, args: &[&str]) -> Result<String, PantheonError> {
            let out = self.git_status(args).ok_or_else(|| {
                err(
                    "WORKTREE_GIT",
                    "failed to run git".to_string(),
                    "ensure git is installed and on PATH",
                )
            })?;
            if !out.status.success() {
                return Err(err(
                    "WORKTREE_GIT",
                    format!(
                        "git {} failed: {}",
                        args.join(" "),
                        String::from_utf8_lossy(&out.stderr).trim()
                    ),
                    "fix the git error and retry",
                ));
            }
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        }

        fn git_status(&self, args: &[&str]) -> Option<std::process::Output> {
            Command::new("git")
                .current_dir(&self.repo)
                .args(args)
                .output()
                .ok()
        }
    }

    /// The first `worktree` entry in `git worktree list --porcelain` is the
    /// repository's primary working tree. Returns `None` when git cannot
    /// report one (then the caller falls back to the current root).
    fn main_worktree(repo: &Path) -> Option<PathBuf> {
        let out = Command::new("git")
            .current_dir(repo)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let raw = String::from_utf8_lossy(&out.stdout);
        raw.lines()
            .find_map(|line| line.strip_prefix("worktree "))
            .map(PathBuf::from)
    }

    fn validate_name(name: &str) -> Result<(), PantheonError> {
        if name.is_empty() || name.len() > 64 {
            return Err(err(
                "WORKTREE_BAD_NAME",
                "worktree name must be 1..=64 characters".to_string(),
                "use a short name like 'review' or 'feature-x'",
            ));
        }
        if name == "." || name == ".." || name.contains('/') || name.contains('\\') {
            return Err(err(
                "WORKTREE_BAD_NAME",
                format!("invalid worktree name '{name}'"),
                "names may not contain path separators",
            ));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(err(
                "WORKTREE_BAD_NAME",
                format!("invalid worktree name '{name}'"),
                "use letters, digits, dashes, underscores, and dots",
            ));
        }
        Ok(())
    }

    /// Parse `git worktree list --porcelain`. Blocks are separated by blank
    /// lines; the first block is the main worktree.
    fn parse_porcelain(raw: &str) -> Vec<Worktree> {
        let mut out: Vec<Worktree> = Vec::new();
        let mut cur: Option<Worktree> = None;
        for line in raw.lines() {
            if line.trim().is_empty() {
                if let Some(w) = cur.take() {
                    out.push(w);
                }
                continue;
            }
            if let Some(path) = line.strip_prefix("worktree ") {
                if let Some(w) = cur.take() {
                    out.push(w);
                }
                cur = Some(Worktree {
                    path: PathBuf::from(path),
                    head: String::new(),
                    branch: None,
                    bare: false,
                    detached: false,
                    main: out.is_empty() && cur.is_none(),
                    locked: false,
                    prunable: false,
                });
                continue;
            }
            let Some(w) = cur.as_mut() else {
                continue;
            };
            if let Some(head) = line.strip_prefix("HEAD ") {
                w.head = head.to_string();
            } else if let Some(branch) = line.strip_prefix("branch ") {
                w.branch = Some(
                    branch
                        .strip_prefix("refs/heads/")
                        .unwrap_or(branch)
                        .to_string(),
                );
            } else if line == "bare" {
                w.bare = true;
            } else if line == "detached" {
                w.detached = true;
            } else if line.starts_with("locked") {
                w.locked = true;
            } else if line.starts_with("prunable") {
                w.prunable = true;
            }
        }
        if let Some(w) = cur.take() {
            out.push(w);
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rejects_unsafe_names() {
            assert!(validate_name("").is_err());
            assert!(validate_name("../escape").is_err());
            assert!(validate_name("a/b").is_err());
            assert!(validate_name("a\\b").is_err());
            assert!(validate_name("with space").is_err());
            assert!(validate_name("ok-name_1.2").is_ok());
        }

        #[test]
        fn parses_porcelain_with_main_and_linked() {
            let raw = "\
worktree /repo
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /state/worktrees/repo-abcd/review
HEAD 2222222222222222222222222222222222222222
branch refs/heads/pantheon/review
locked reason

";
            let list = parse_porcelain(raw);
            assert_eq!(list.len(), 2);
            assert!(list[0].main);
            assert!(!list[0].detached);
            assert_eq!(list[0].branch.as_deref(), Some("main"));
            assert!(!list[1].main);
            assert!(list[1].locked);
            assert_eq!(list[1].pantheon_branch(), Some("pantheon/review"));
        }

        #[test]
        fn create_list_remove_round_trip() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let repo = tmp.path().join("repo");
            std::fs::create_dir_all(&repo).expect("mkdir");
            let run = |args: &[&str]| {
                let out = Command::new("git")
                    .current_dir(&repo)
                    .args(args)
                    .output()
                    .expect("git");
                assert!(
                    out.status.success(),
                    "git {:?}: {}",
                    args,
                    String::from_utf8_lossy(&out.stderr)
                );
            };
            run(&["init", "-q"]);
            run(&["config", "user.email", "test@example.com"]);
            run(&["config", "user.name", "Test"]);
            std::fs::write(repo.join("README.md"), "hello\n").expect("write");
            run(&["add", "-A"]);
            run(&["commit", "-q", "-m", "init"]);

            let state = tmp.path().join("state");
            let m = WorktreeManager::new(&repo, &state).expect("manager");
            let wt = m.create("review", None, None).expect("create");
            assert!(wt.path.exists());
            assert_eq!(wt.branch.as_deref(), Some("pantheon/review"));
            assert!(!wt.main);

            let all = m.list().expect("list");
            assert_eq!(all.len(), 2);
            assert!(all[0].main);

            // Running from inside the linked worktree must still resolve the
            // repository's primary tree, so managed paths share one namespace.
            let from_linked = WorktreeManager::new(&wt.path, &state).expect("manager from linked");
            assert_eq!(from_linked.root(), m.root());
            assert_eq!(from_linked.list().expect("list").len(), 2);

            // While the directory exists, a second create with the same name
            // fails with git's path-exists error.
            assert!(m.create("review", None, None).is_err());

            let removed = m.remove("review", false).expect("remove");
            assert_eq!(removed.branch.as_deref(), Some("pantheon/review"));
            assert!(!m.list().expect("list").iter().any(|w| !w.main));

            // The branch outlives the worktree; recreating attaches it instead
            // of failing on an existing ref.
            let again = m.create("review", None, None).expect("recreate");
            assert_eq!(again.branch.as_deref(), Some("pantheon/review"));
            m.remove("review", false).expect("remove recreated");

            // An explicit --path is still addressable by name (the branch is
            // in Pantheon's namespace) and removable safely.
            let custom = tmp.path().join("elsewhere");
            let made = m
                .create("custom", None, Some(&custom))
                .expect("create custom");
            assert_eq!(made.path, custom);
            let found = m.find("custom").expect("find").expect("custom worktree");
            assert_eq!(found.path, custom);
            m.remove("custom", false).expect("remove custom");
            assert!(!custom.exists());
        }
    }
}
pub mod process {
    //! Process-group lifecycle helpers.
    //!
    //! Cancellation is deliberately a two-step action: persist `canceling` in
    //! the operation, then TERM the owned process group, wait briefly, and KILL
    //! only that group.  PID numbers alone are never treated as ownership.

    use std::time::Duration;

    #[cfg(unix)]
    use std::time::Instant;

    #[cfg(unix)]
    const SIGTERM: i32 = 15;
    #[cfg(unix)]
    const SIGKILL: i32 = 9;

    /// An owned process group.  Constructing this from a persisted group row is
    /// safe because the row is tied to a live run lease by the runtime.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ProcessGroup {
        pub pgid: i32,
    }

    impl ProcessGroup {
        pub fn new(pgid: i32) -> Option<Self> {
            (pgid > 1).then_some(Self { pgid })
        }

        /// TERM → grace period → KILL.  The guard rejects PID 1 and our own PID.
        pub fn terminate(&self, grace: Duration) {
            let Some(group) = Self::new(self.pgid) else {
                return;
            };
            if group.pgid == std::process::id() as i32 {
                return;
            }
            #[cfg(unix)]
            {
                unsafe {
                    // Ignore ESRCH: the desired terminal state is already true.
                    libc::killpg(group.pgid, SIGTERM);
                }
                let deadline = Instant::now() + grace;
                while Instant::now() < deadline {
                    // A group is considered gone when killpg returns ESRCH.  We
                    // do not use `kill(0, ...)` because that would probe the
                    // caller's own group.
                    #[cfg(target_os = "linux")]
                    unsafe {
                        if libc::killpg(group.pgid, 0) < 0 {
                            let e = std::io::Error::last_os_error();
                            if e.raw_os_error() == Some(libc::ESRCH) {
                                return;
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                unsafe {
                    libc::killpg(group.pgid, SIGKILL);
                }
            }
            #[cfg(not(unix))]
            {
                // Non-Unix implementations use the process id as the group id.
                // The caller still has to provide a valid child pid; no shell is
                // spawned here.
                let _ = std::process::Command::new("kill")
                    .args(["-KILL", &group.pgid.to_string()])
                    .status();
            }
        }
    }
}
pub mod context {
    //! Context-window budget (sibling of output compaction): deterministic
    //! fitting of the assembled transcript to the model's window BEFORE the
    //! provider call, so long runs degrade by dropping the oldest context
    //! instead of dying on a provider 400.
    //!
    //! Rules (all deterministic, no model in the loop):
    //! 1. Under budget: nothing changes.
    //! 2. Re-compact the oldest oversized tool rows to a hard floor (data goes,
    //!    instructions stay - same head+tail rule as `compact_output`).
    //! 3. Drop oldest *whole exchanges* (a user row plus every row up to the
    //!    next user row). System rows, the preamble before the first user row,
    //!    and the final exchange are never dropped - dropping whole exchanges
    //!    keeps assistant/tool_call pairing valid on the wire.
    //! 4. Still over with only essential rows left: structured
    //!    `CONTEXT_OVERFLOW` (host fixes the prompt or picks a bigger window).
    //!
    //! The estimate is bytes/4 plus per-row overhead - an estimate, padded by a
    //! safety factor. It is not a tokenizer; it only has to trigger trimming
    //! before the provider rejects the request.

    use crate::{compact_output, CompactionPolicy};
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::message::{Message, Role};
    use pantheon_api::model::{CompressionRequest, ContextCompressor};

    /// Tool rows are re-compacted down to this floor (bytes) in step 2.
    pub const TOOL_FLOOR_BYTES: usize = 2 * 1024;
    /// Estimator margin: fit to 85% of the usable window, since bytes/4 is
    /// approximate and providers count wire overhead we cannot see.
    pub const DEFAULT_SAFETY: f32 = 0.85;

    /// Rough token estimate for a text blob: bytes/4, rounded up.
    pub fn estimate_tokens(text: &str) -> u32 {
        (text.len() as u32).div_ceil(4)
    }

    /// Estimated tokens for one row: content + fixed row overhead + tool-call
    /// payloads (they ride as JSON on the wire, so they must be counted) +
    /// image parts (OpenAI-style high-detail pricing: 85 base + 170 per 512px
    /// tile; without decoded dimensions we estimate ~1 tile per 256 KiB as a
    /// rough proxy - this is a window-fitting estimate, not a bill).
    pub fn row_tokens(m: &Message) -> u32 {
        let mut t = estimate_tokens(&m.content) + 8;
        for c in &m.tool_calls {
            t += estimate_tokens(&c.name) + estimate_tokens(&c.arguments) + 16;
        }
        for img in &m.images {
            let bytes = img.data.len() * 3 / 4;
            let tiles = (bytes / (256 * 1024)).max(1) as u32;
            t += 85 + 170 * tiles;
        }
        t
    }

    /// Estimated input tokens for a whole transcript.
    pub fn estimate_messages(messages: &[Message]) -> u32 {
        messages
            .iter()
            .map(row_tokens)
            .fold(0u32, |a, b| a.saturating_add(b))
    }

    /// What the fit may spend on input tokens.
    #[derive(Debug, Clone, PartialEq)]
    pub struct WindowBudget {
        /// Full context window (catalog `context_limit`).
        pub limit: u32,
        /// Tokens reserved for the completion (catalog `max_output_tokens`).
        pub reserve_output: u32,
        /// Multiplier applied to the remainder (estimator margin).
        pub safety: f32,
    }

    impl WindowBudget {
        pub fn new(limit: u32, reserve_output: u32) -> Self {
            Self {
                limit,
                reserve_output,
                safety: DEFAULT_SAFETY,
            }
        }

        /// Usable input tokens after reserving output room and padding.
        pub fn usable(&self) -> u32 {
            let avail = self.limit.saturating_sub(self.reserve_output) as f32;
            (avail * self.safety) as u32
        }
    }

    /// What the fit did, for the ledger event.
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct FitReport {
        /// Estimated input tokens after fitting.
        pub estimated: u32,
        /// The usable window it fitted to.
        pub window: u32,
        /// Rows dropped (oldest whole exchanges).
        pub dropped_rows: u32,
        /// Tool rows re-compacted to the floor.
        pub compacted_rows: u32,
    }

    impl FitReport {
        /// True when the transcript the model sees differs from the original.
        pub fn changed(&self) -> bool {
            self.dropped_rows > 0 || self.compacted_rows > 0
        }
    }

    /// Fit `messages` into `budget`. Returns the (possibly trimmed) transcript
    /// plus a report; `CONTEXT_OVERFLOW` when even the essential rows do not fit.
    pub fn fit_to_window(
        mut messages: Vec<Message>,
        budget: &WindowBudget,
    ) -> Result<(Vec<Message>, FitReport), PantheonError> {
        let window = budget.usable();
        let mut estimated = estimate_messages(&messages);
        let mut report = FitReport {
            estimated,
            window,
            ..Default::default()
        };
        if estimated <= window {
            return Ok((messages, report));
        }

        // Step 1: re-compact the oldest oversized tool rows to the floor.
        let floor = CompactionPolicy {
            max_lines: 40,
            head_lines: 15,
            max_bytes: TOOL_FLOOR_BYTES,
        };
        for m in messages.iter_mut() {
            if estimated <= window {
                break;
            }
            if m.role != Role::Tool || m.content.len() <= TOOL_FLOOR_BYTES {
                continue;
            }
            let before = row_tokens(m);
            let c = compact_output(&m.content, &floor);
            if c.truncated {
                m.content = c.text;
                let after = row_tokens(m);
                estimated = estimated.saturating_sub(before.saturating_sub(after));
                report.compacted_rows += 1;
            }
        }
        report.estimated = estimated;
        if estimated <= window {
            return Ok((messages, report));
        }

        // Step 2: drop oldest whole exchanges (user row .. next user row).
        // The final exchange is always kept: it carries the live turn and any
        // pending assistant/tool pairing the loop must settle.
        let starts: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::User)
            .map(|(i, _)| i)
            .collect();
        let mut drop_until = 0usize;
        for k in 0..starts.len().saturating_sub(1) {
            if estimated <= window {
                break;
            }
            let end = starts[k + 1];
            let ex: u32 = messages[starts[k]..end].iter().map(row_tokens).sum();
            estimated = estimated.saturating_sub(ex);
            report.dropped_rows += (end - starts[k]) as u32;
            drop_until = end;
        }
        if let Some(&first_user) = starts.first() {
            // Drain only the dropped exchanges; rows before the first user
            // row (system preamble) are never dropped.
            if drop_until > first_user {
                messages.drain(first_user..drop_until);
            }
        }
        report.estimated = estimated;

        // Step 3: essential rows (system preamble + final exchange) still over.
        if estimated > window {
            return Err(PantheonError::new(
            "CONTEXT_OVERFLOW",
            Layer::Runtime,
            false,
            format!(
                "context is ~{estimated} tokens after trimming, window allows {window}"
            ),
            "shorten the system prompt or extension context, or use a model with a larger context window",
            "",
        ));
        }
        Ok((messages, report))
    }

    /// Per-row cap when rendering the compression transcript: old tool rows can
    /// be far above the fit floor (they were compacted per-call, not per-fit).
    pub const RENDER_ROW_CHARS: usize = 2_000;
    /// Absolute cap on the rendered transcript, protecting the aux endpoint.
    pub const RENDER_MAX_CHARS: usize = 100_000;
    /// Summary floor: below this a summary is noise, not signal.
    pub const SUMMARY_MIN_CHARS: usize = 128;
    /// Default summary target as a percentage of the absorbed transcript
    /// chars (`[compression] target_percent` when set). 12 ≈ the historic
    /// fixed target: an eighth of the absorbed tokens at ~4 chars/token.
    pub const DEFAULT_TARGET_PERCENT: u8 = 12;

    /// Summary budget in chars for `chunk_tokens` of absorbed transcript:
    /// `target_percent` (1-100) of the absorbed chars, assuming ~4
    /// chars/token. `None` = [`DEFAULT_TARGET_PERCENT`]. Higher keeps more
    /// detail (less aggressive); lower compresses harder.
    pub fn summary_target_chars(chunk_tokens: u32, target_percent: Option<u8>) -> usize {
        let percent = target_percent.unwrap_or(DEFAULT_TARGET_PERCENT) as usize;
        (chunk_tokens as usize) * 4 * percent / 100
    }

    /// What one compression pass absorbed, for the ledger event.
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct CompressionReport {
        /// Exchanges absorbed into the summary.
        pub exchanges: u32,
        /// Rows removed (replaced by one summary row).
        pub rows: u32,
        /// Rendered transcript size in chars.
        pub chars_before: u32,
        /// Summary row size in chars.
        pub chars_after: u32,
        /// The summary text persisted in the ledger event so resumed turns
        /// do not re-pay the compression call.
        pub summary: String,
    }

    /// Render rows `range` as role-tagged text for the compression model,
    /// capping each row at `RENDER_ROW_CHARS` and the whole render at
    /// `RENDER_MAX_CHARS` (truncation returns what fits).
    pub fn render_exchanges(messages: &[Message], range: std::ops::Range<usize>) -> String {
        let mut out = String::new();
        for m in &messages[range] {
            let role = match m.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            let mut line = String::from(role);
            line.push_str(": ");
            // Image parts cannot survive as pixels in the compressor's text
            // input: name them (`[image: ...]`) instead of silently dropping
            // the fact that a picture was here.
            let noted = m.text_with_image_notes();
            let content: String = noted.chars().take(RENDER_ROW_CHARS).collect();
            line.push_str(&content);
            if noted.chars().count() > RENDER_ROW_CHARS {
                line.push_str(" [...]");
            }
            line.push('\n');
            // Bound before appending: a row must never push the render past
            // the cap (checking after would overshoot by up to a full row).
            if out.len() + line.len() > RENDER_MAX_CHARS {
                out.push_str("[... render cap ...]\n");
                break;
            }
            out.push_str(&line);
        }
        out
    }

    /// Overflow-triggered compression of the oldest exchanges via the host's
    /// compression aux model - the model-assisted step BETWEEN deterministic
    /// tool compaction and deterministic dropping (exec itself never talks to
    /// a model; the host passes a `ContextCompressor` implementation).
    ///
    /// - Under budget: `Ok(None)`, nothing touched.
    /// - Over budget: oldest exchanges (never the final one, never the system
    ///   preamble) are summarized into one memory-tier note spliced where the
    ///   absorbed exchanges stood. The caller re-runs [`fit_to_window`] after;
    ///   any remainder (or a compressor error) falls back to deterministic
    ///   dropping.
    ///
    /// `target_percent` (1-100, `None` = [`DEFAULT_TARGET_PERCENT`]) sets the
    /// summary size as a percentage of the absorbed transcript chars.
    pub fn compress_oldest(
        messages: &[Message],
        compressor: &dyn ContextCompressor,
        budget: &WindowBudget,
        run_id: &str,
        target_percent: Option<u8>,
    ) -> Result<Option<(Vec<Message>, CompressionReport)>, PantheonError> {
        let window = budget.usable();
        let estimated = estimate_messages(messages);
        if estimated <= window {
            return Ok(None);
        }

        // Exchange boundaries: a user row through the row before the next user
        // row. The final exchange is the live turn - never absorbed. Anything
        // before the first user row is the system preamble - never absorbed.
        let starts: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::User)
            .map(|(i, _)| i)
            .collect();
        if starts.len() < 2 {
            return Ok(None); // no droppable exchange to compress
        }

        // Absorb oldest exchanges until the material covers the overflow (plus
        // a tenth of the window as slack so one pass usually suffices).
        let needed = estimated.saturating_sub(window.saturating_sub(window / 10));
        let mut chunk_end = starts[0];
        let mut chunk_tokens = 0u32;
        let mut exchanges = 0u32;
        for k in 0..starts.len() - 1 {
            if chunk_tokens >= needed && exchanges > 0 {
                break;
            }
            let end = starts[k + 1];
            if chunk_tokens > 0
                && render_exchanges(messages, starts[0]..end).len() >= RENDER_MAX_CHARS
            {
                break; // input cap: leave the rest to the deterministic drop
            }
            let ex: u32 = messages[chunk_end..end].iter().map(row_tokens).sum();
            chunk_tokens += ex;
            chunk_end = end;
            exchanges += 1;
        }
        if exchanges == 0 {
            return Ok(None);
        }

        let range = starts[0]..chunk_end;
        let transcript = render_exchanges(messages, range.clone());
        if transcript.trim().is_empty() {
            return Ok(None);
        }

        // Summary budget: `target_percent` of the absorbed material,
        // floored so the note carries real signal, never more than half the
        // input.
        let target_chars = summary_target_chars(chunk_tokens, target_percent)
            .max(SUMMARY_MIN_CHARS)
            .min(transcript.len() / 2)
            .max(SUMMARY_MIN_CHARS);
        let req = CompressionRequest {
            run_id: run_id.to_string(),
            transcript,
            target_chars,
        };
        let result = compressor.compress(&req)?;
        let summary = result.summary.trim();
        if summary.is_empty() {
            return Err(PantheonError::new(
                "COMPRESSION_EMPTY",
                Layer::Execution,
                true,
                "compression model returned an empty summary".to_string(),
                "the deterministic fit still runs; check the [compression] endpoint",
                "",
            ));
        }

        // Memory-tier context: informative, never authoritative. Providers
        // render the envelope so the model sees the trust inline.
        let note = Message::system(format!(
            "<compressed_context>\n{summary}\n</compressed_context>"
        ))
        .with_provenance(pantheon_api::provenance::Provenance::memory(
            "context-compression",
        ));
        let chars_after = note.content.len() as u32;
        let chars_before = messages[range.clone()]
            .iter()
            .map(|m| m.content.len() as u32)
            .sum();

        let mut out = Vec::with_capacity(messages.len());
        out.extend_from_slice(&messages[..range.start]);
        out.push(note);
        out.extend_from_slice(&messages[range.end..]);
        Ok(Some((
            out,
            CompressionReport {
                exchanges,
                rows: (range.end - range.start) as u32,
                chars_before,
                chars_after,
                summary: summary.to_string(),
            },
        )))
    }
}
