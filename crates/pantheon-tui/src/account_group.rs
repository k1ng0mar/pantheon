//! account_group: secrets, secrets_form, oauth, backup, checkpoint in one file.

pub mod secrets {
    //! `pantheon secrets migrate`: move plaintext dotenv stores into the encrypted vault.
    //!
    //! Backup first, migrate, verify by reopening, never delete plaintext.
    //! Deleting plaintext is a separate human approved step after restart.

    use std::path::PathBuf;

    fn flag_val(args: &[String], name: &str) -> Option<String> {
        let mut it = args.iter().peekable();
        while let Some(a) = it.next() {
            if a == name {
                return it.next().cloned();
            }
            if let Some(v) = a.strip_prefix(&format!("{name}=")) {
                return Some(v.to_string());
            }
        }
        None
    }

    pub fn cmd_secrets(args: &[String]) {
        if args.iter().any(|a| a == "--help" || a == "-h") || args.len() < 3 {
            println!("usage: pantheon secrets migrate [--file FILE] [--dir DIR]");
            println!("  migrate gateway.env and logins.env into the encrypted vault");
            println!("  backup is written before migration, plaintext is left in place");
            return;
        }
        match args[2].as_str() {
            "migrate" => {
                let data_dir = flag_val(args, "--dir")
                    .map(PathBuf::from)
                    .unwrap_or_else(crate::terminal::data_dir);
                let files: Vec<String> = if let Some(f) = flag_val(args, "--file") {
                    vec![f]
                } else {
                    vec!["gateway.env".to_string(), "logins.env".to_string()]
                };
                let vault = data_dir.join("secrets.json");
                let key = data_dir.join(".secrets.key");
                for file in &files {
                    if !data_dir.join(file).exists() {
                        println!("{file}: not present, skipping");
                        continue;
                    }
                    match pantheon_secrets::migration::migrate_dotenv_to_filevault(
                        &data_dir, file, &vault, &key,
                    ) {
                        Ok(report) => {
                            println!(
                                "{}: migrated {} entries, verified {}",
                                report.file,
                                report.migrated,
                                if report.verified { "yes" } else { "no" }
                            );
                        }
                        Err(e) => {
                            eprintln!("{file}: migration failed: {e}");
                            std::process::exit(1);
                        }
                    }
                }
                println!(
                    "plaintext originals left in place. delete only after a verified restart."
                );
            }
            other => {
                eprintln!("secrets: unknown subcommand {other}");
                eprintln!("usage: pantheon secrets migrate [--file FILE] [--dir DIR]");
                std::process::exit(2);
            }
        }
    }
}

pub mod secrets_form {
    //! Secure secret entry form for the TUI.
    //!
    //! The form is a small state machine over plain data, so it is testable
    //! without a terminal. The session layer maps crossterm keys onto
    //! [`FormKey`] and renders [`SecretsForm::render_lines`] into an overlay.
    //!
    //! Security contract, enforced here and nowhere else:
    //!
    //! * The value is held only as a [`pantheon_secrets::SecretValue`]
    //!   (zeroizing, `Debug` shows `***`).
    //! * [`MaskedInput::render`] shows one bullet per character and never the
    //!   value. [`SecretsForm::render_lines`] never includes the value.
    //! * The value is never written to the transcript, the status line, or a
    //!   log: the session submits it straight to
    //!   [`pantheon_secrets::SecureFormStore`] and the form drops it on
    //!   close, cancel, or submit.

    use pantheon_secrets::{SecretScope, SecretValue};

    /// One key, in form terms. The session maps crossterm events onto this.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FormKey {
        Char(char),
        Backspace,
        Delete,
        Enter,
        Esc,
        Tab,
        Up,
        Down,
    }

    /// Masked single-line input. Rendering shows bullets only.
    pub struct MaskedInput {
        value: SecretValue,
        len: usize,
    }

    impl Default for MaskedInput {
        fn default() -> Self {
            Self {
                value: SecretValue::new(String::new()),
                len: 0,
            }
        }
    }

    impl std::fmt::Debug for MaskedInput {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "MaskedInput(***)")
        }
    }

    impl MaskedInput {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn push(&mut self, c: char) {
            let mut s = self.value.expose().to_string();
            s.push(c);
            self.len = s.chars().count();
            self.value = SecretValue::new(s);
        }

        pub fn backspace(&mut self) {
            let mut s = self.value.expose().to_string();
            if s.pop().is_some() {
                self.len = s.chars().count();
                self.value = SecretValue::new(s);
            }
        }

        pub fn clear(&mut self) {
            self.value = SecretValue::new(String::new());
            self.len = 0;
        }

        pub fn is_empty(&self) -> bool {
            self.len == 0
        }

        /// Bullets only. Never the value, never a prefix of it.
        pub fn render(&self) -> String {
            "•".repeat(self.len)
        }

        /// Consume the value for submission. The input is empty afterwards.
        pub fn take(&mut self) -> SecretValue {
            let v = std::mem::replace(&mut self.value, SecretValue::new(String::new()));
            self.len = 0;
            v
        }
    }

    /// Which field the form is on.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FormStep {
        Name,
        Scope,
        Value,
        /// Verification ran; the form shows the outcome and offers retry
        /// (back to Value) or done. Set by the session after submission.
        Outcome,
    }

    /// Outcome the session reports back into the form after submitting.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum SubmitOutcome {
        Verified,
        Failed(String),
        Unavailable(String),
        Error(String),
    }

    /// The secure entry form itself.
    pub struct SecretsForm {
        pub name: String,
        pub scope_index: usize,
        pub value: MaskedInput,
        pub step: FormStep,
        pub outcome: Option<SubmitOutcome>,
        /// Set when the user confirms submission; the session reads the
        /// value out with [`SecretsForm::take_submission`] and clears it.
        pub submit_ready: bool,
    }

    impl std::fmt::Debug for SecretsForm {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("SecretsForm")
                .field("name", &self.name)
                .field("scope", &self.scope())
                .field("step", &self.step)
                .field("value_len", &self.value.len)
                .finish()
        }
    }

    impl Default for SecretsForm {
        fn default() -> Self {
            Self::new()
        }
    }

    impl SecretsForm {
        pub fn new() -> Self {
            Self {
                name: String::new(),
                scope_index: 0,
                value: MaskedInput::new(),
                step: FormStep::Name,
                outcome: None,
                submit_ready: false,
            }
        }

        /// A form prefilled for rotating an existing secret: the name and
        /// scope are already set, so the user only types the new value.
        pub fn prefilled(name: &str, scope: SecretScope) -> Self {
            let mut form = Self::new();
            form.name = name.to_string();
            form.scope_index = SecretScope::all()
                .iter()
                .position(|s| *s == scope)
                .unwrap_or(0);
            form.step = FormStep::Value;
            form
        }

        pub fn scope(&self) -> SecretScope {
            SecretScope::all()[self.scope_index % SecretScope::all().len()]
        }

        fn advance(&mut self) {
            self.step = match self.step {
                FormStep::Name => FormStep::Scope,
                FormStep::Scope => FormStep::Value,
                FormStep::Value => FormStep::Value,
                FormStep::Outcome => FormStep::Outcome,
            };
        }

        fn retreat(&mut self) {
            self.step = match self.step {
                FormStep::Name => FormStep::Name,
                FormStep::Scope => FormStep::Name,
                FormStep::Value => FormStep::Scope,
                FormStep::Outcome => FormStep::Value,
            };
        }

        /// Handle one key. Returns true when the form should close
        /// (Esc, or Enter on the outcome step).
        pub fn on_key(&mut self, key: FormKey) -> bool {
            match key {
                FormKey::Esc => return true,
                FormKey::Tab => {
                    if self.step == FormStep::Scope {
                        self.scope_index = (self.scope_index + 1) % SecretScope::all().len();
                    } else {
                        self.advance();
                    }
                    return false;
                }
                FormKey::Up => {
                    if self.step == FormStep::Scope {
                        self.scope_index = self
                            .scope_index
                            .checked_sub(1)
                            .unwrap_or(SecretScope::all().len() - 1);
                    }
                    return false;
                }
                FormKey::Down => {
                    if self.step == FormStep::Scope {
                        self.scope_index = (self.scope_index + 1) % SecretScope::all().len();
                    }
                    return false;
                }
                FormKey::Enter => {
                    match self.step {
                        FormStep::Name => {
                            if !self.name.trim().is_empty() {
                                self.advance();
                            }
                        }
                        FormStep::Scope => self.advance(),
                        FormStep::Value => {
                            if !self.value.is_empty() {
                                self.submit_ready = true;
                            }
                        }
                        FormStep::Outcome => return true,
                    }
                    return false;
                }
                FormKey::Backspace => match self.step {
                    FormStep::Name => {
                        self.name.pop();
                    }
                    FormStep::Value => self.value.backspace(),
                    _ => self.retreat(),
                },
                FormKey::Delete => match self.step {
                    FormStep::Name => {
                        self.name.clear();
                    }
                    FormStep::Value => self.value.clear(),
                    _ => {}
                },
                FormKey::Char(c) => match self.step {
                    FormStep::Name => {
                        if c.is_ascii_alphanumeric() || c == '_' {
                            self.name.push(c);
                        }
                    }
                    FormStep::Value => self.value.push(c),
                    _ => {}
                },
            }
            false
        }

        /// Take the submission out of the form. Returns None unless Enter
        /// was pressed on the Value step with a non-empty value.
        pub fn take_submission(&mut self) -> Option<(String, SecretScope, SecretValue)> {
            if !self.submit_ready {
                return None;
            }
            self.submit_ready = false;
            self.step = FormStep::Outcome;
            Some((
                self.name.trim().to_string(),
                self.scope(),
                self.value.take(),
            ))
        }

        /// Feed a verification outcome back into the form for display.
        pub fn set_outcome(&mut self, outcome: SubmitOutcome) {
            self.outcome = Some(outcome);
            self.step = FormStep::Outcome;
        }

        /// Retry after a failed verification: back to the Value step with a
        /// cleared value, so the failed value is never re-submitted by
        /// accident and never displayed.
        pub fn retry(&mut self) {
            self.value.clear();
            self.outcome = None;
            self.step = FormStep::Value;
        }

        /// Render lines for the overlay. The value never appears: only
        /// bullets, and only its length in bullet form.
        pub fn render_lines(&self) -> Vec<String> {
            let mut lines = vec![
                "Secure secret entry (values are never shown or recorded)".to_string(),
                format!("Name (env var name): {}", self.name),
                format!("Scope: {}  (Tab/Up/Down to change)", self.scope().as_str()),
                format!("Value: {}", self.value.render()),
            ];
            match self.step {
                FormStep::Name => lines.push("Type the secret name, Enter to continue.".to_string()),
                FormStep::Scope => lines.push("Pick a scope, Enter to continue.".to_string()),
                FormStep::Value => {
                    lines.push("Type the secret (masked), Enter to save and verify.".to_string())
                }
                FormStep::Outcome => match &self.outcome {
                    Some(SubmitOutcome::Verified) => {
                        lines.push("Saved and verified. Enter to close.".to_string())
                    }
                    Some(SubmitOutcome::Failed(reason)) => lines.push(format!(
                        "Saved, but verification failed: {reason}. Press r to retry (value cleared) or Enter to close."
                    )),
                    Some(SubmitOutcome::Unavailable(reason)) => {
                        lines.push(format!("Saved. No verification available: {reason}. Enter to close."))
                    }
                    Some(SubmitOutcome::Error(reason)) => {
                        lines.push(format!("Not saved: {reason}. Press r to retry or Enter to close."))
                    }
                    None => lines.push("Enter to close.".to_string()),
                },
            }
            lines.push(
                "Esc cancels. The value is stored in the secrets broker, never in config."
                    .to_string(),
            );
            lines
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn masked_input_never_renders_the_value() {
            let mut input = MaskedInput::new();
            for c in "hunter2-secret".chars() {
                input.push(c);
            }
            let rendered = input.render();
            assert_eq!(rendered, "•".repeat(14));
            assert!(!rendered.contains("hunter"));
            let dbg = format!("{input:?}");
            assert!(!dbg.contains("hunter"));
        }

        #[test]
        fn form_flow_name_scope_value_submit() {
            let mut form = SecretsForm::new();
            for c in "MY_KEY".chars() {
                form.on_key(FormKey::Char(c));
            }
            form.on_key(FormKey::Enter);
            assert_eq!(form.step, FormStep::Scope);
            form.on_key(FormKey::Enter);
            assert_eq!(form.step, FormStep::Value);
            for c in "s3cr3t".chars() {
                form.on_key(FormKey::Char(c));
            }
            form.on_key(FormKey::Enter);
            let (name, scope, value) = form.take_submission().expect("submission");
            assert_eq!(name, "MY_KEY");
            assert_eq!(scope, SecretScope::Provider);
            assert_eq!(value.expose(), "s3cr3t");
            assert!(form.value.is_empty());
        }

        #[test]
        fn form_render_never_contains_value() {
            let mut form = SecretsForm::new();
            for c in "MY_KEY".chars() {
                form.on_key(FormKey::Char(c));
            }
            form.on_key(FormKey::Enter);
            form.on_key(FormKey::Enter);
            for c in "do-not-show-me".chars() {
                form.on_key(FormKey::Char(c));
            }
            let joined = form.render_lines().join("\n");
            assert!(!joined.contains("do-not-show-me"));
            assert!(joined.contains("•"));
        }

        #[test]
        fn retry_clears_value_and_returns_to_value_step() {
            let mut form = SecretsForm::new();
            for c in "K".chars() {
                form.on_key(FormKey::Char(c));
            }
            form.on_key(FormKey::Enter);
            form.on_key(FormKey::Enter);
            for c in "bad".chars() {
                form.on_key(FormKey::Char(c));
            }
            form.on_key(FormKey::Enter);
            let _ = form.take_submission();
            form.set_outcome(SubmitOutcome::Failed(
                "provider returned HTTP 401".to_string(),
            ));
            form.retry();
            assert_eq!(form.step, FormStep::Value);
            assert!(form.value.is_empty());
            assert!(form.outcome.is_none());
        }

        #[test]
        fn name_rejects_non_env_characters() {
            let mut form = SecretsForm::new();
            for c in "A-B C".chars() {
                form.on_key(FormKey::Char(c));
            }
            assert_eq!(form.name, "ABC");
        }
    }
}

pub mod oauth {
    //! `pantheon oauth`: connect hosted apps via local OAuth2.
    //!
    //! Tokens live in the local durable vaults (OS keychain, else the
    //! encrypted file vault): there is no server side. Client IDs come from
    //! `PANTHEON_OAUTH_<APP>_CLIENT_ID`; every flow is the RFC 8252
    //! public-client loopback flow with PKCE S256, so no client secret is
    //! needed (Notion additionally honors `PANTHEON_OAUTH_NOTION_CLIENT_SECRET`
    //! for confidential integrations).
    //!
    //! Nothing here prints token material: status shows presence, scopes and
    //! expiry only.

    use pantheon_secrets::{oauth, SecretsBroker};
    use std::time::{Duration, SystemTime};

    /// `pantheon oauth <login|status|logout> [app]`. `args[0]` is argv[0],
    /// `args[1]` the verb, `args[2]` the subcommand - same shape as
    /// `cmd_gateway`.
    pub fn cmd_oauth(args: &[String]) {
        match args.get(2).map(String::as_str) {
            Some("login") => cmd_login(args.get(3).map(String::as_str)),
            Some("status") => cmd_status(),
            Some("logout") => cmd_logout(args.get(3).map(String::as_str)),
            _ => {
                eprintln!("usage: pantheon oauth <login|status|logout> [app]");
                eprintln!("  apps: github, google, notion, spotify");
                std::process::exit(2);
            }
        }
    }

    fn broker() -> SecretsBroker {
        SecretsBroker::durable(&crate::terminal::data_dir()).unwrap_or_else(|e| {
            eprintln!("oauth: cannot open secret store: {e}");
            std::process::exit(1);
        })
    }

    /// Non-exiting variants for the TUI: the CLI helpers above print and
    /// `process::exit` on error, which would kill (or corrupt the screen
    /// of) a running TUI. The TUI dispatch arm uses these and reports
    /// through transcript status lines instead.
    fn broker_result() -> Result<SecretsBroker, String> {
        SecretsBroker::durable(&crate::terminal::data_dir())
            .map_err(|e| format!("cannot open secret store: {e}"))
    }

    fn resolve_app_result(app: Option<&str>) -> Result<oauth::OAuthProvider, String> {
        let id = app.unwrap_or("").to_lowercase();
        oauth::provider(&id).ok_or_else(|| {
            format!("unknown oauth app '{id}'; choose github, google, notion, or spotify")
        })
    }

    /// One status line per provider, or an error string. Shared by the
    /// CLI `status` (which prints) and the TUI `/oauth status` arm.
    pub fn status_lines() -> Result<Vec<String>, String> {
        let broker = broker_result()?;
        let store = oauth::OAuthStore::new(&broker);
        let mut out = Vec::new();
        for prov in oauth::PROVIDERS {
            match store.status(prov.id) {
                Ok(s) if s.connected => {
                    let scopes = if s.scopes.is_empty() {
                        "(none required)".to_string()
                    } else {
                        s.scopes.join(" ")
                    };
                    out.push(format!(
                        "{}: connected (scopes: {scopes}; expires {})",
                        prov.id,
                        describe_expiry(s.expires_at)
                    ));
                }
                Ok(_) => out.push(format!("{}: not connected", prov.id)),
                Err(e) => return Err(format!("{}: status error: {e}", prov.id)),
            }
        }
        Ok(out)
    }

    /// Disconnect one app, returning the report line or an error string.
    pub fn logout_line(app: Option<&str>) -> Result<String, String> {
        let prov = resolve_app_result(app)?;
        let broker = broker_result()?;
        let store = oauth::OAuthStore::new(&broker);
        store
            .delete(prov.id)
            .map(|_| format!("{}: disconnected", prov.id))
            .map_err(|e| format!("oauth: logout failed: {e}"))
    }

    fn resolve_app(app: Option<&str>) -> oauth::OAuthProvider {
        let id = app.unwrap_or("").to_lowercase();
        oauth::provider(&id).unwrap_or_else(|| {
            eprintln!("unknown oauth app '{id}'; choose github, google, notion, or spotify");
            std::process::exit(2);
        })
    }

    /// Per-app client registration hints: where the OAuth client comes from
    /// and which redirect URI to allowlist (any 127.0.0.1 port works for the
    /// loopback flow; the exact URI is printed at login time).
    fn registration_hint(prov: oauth::OAuthProvider) -> &'static str {
        match prov.id {
            "github" => "GitHub Settings > Developer settings > OAuth Apps (Authorization callback URL: http://127.0.0.1)",
            "google" => "Google Cloud Console > APIs & Services > Credentials > OAuth client ID, application type Desktop",
            "notion" => "Notion Developers > your integration > Distribution: public, add http://127.0.0.1 as a redirect URI",
            "spotify" => "Spotify Developer Dashboard > your app > Settings > add http://127.0.0.1 as a redirect URI",
            _ => "your provider's developer console",
        }
    }

    fn cmd_login(app: Option<&str>) {
        let prov = resolve_app(app);
        let client_id = std::env::var(prov.client_id_env)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                eprintln!(
                    "missing {}: create an OAuth client first.",
                    prov.client_id_env
                );
                eprintln!("  {}", registration_hint(prov));
                eprintln!(
                    "then run: {}=YOUR_CLIENT_ID pantheon oauth login {}",
                    prov.client_id_env, prov.id
                );
                std::process::exit(2);
            });
        let broker = broker();
        match oauth::login_flow(prov, &client_id) {
            Ok(token) => {
                let store = oauth::OAuthStore::new(&broker);
                if let Err(e) = store.save(prov.id, &token) {
                    eprintln!("oauth: failed to store token: {e}");
                    std::process::exit(1);
                }
                let scopes = if token.scopes.is_empty() {
                    "(none required)".to_string()
                } else {
                    token.scopes.join(" ")
                };
                println!("{} connected (scopes: {scopes})", prov.display);
            }
            Err(e) => {
                eprintln!("oauth: login failed: {e}");
                std::process::exit(1);
            }
        }
    }

    fn describe_expiry(expires_at: Option<SystemTime>) -> String {
        match expires_at {
            None => "never".to_string(),
            Some(t) => match t.duration_since(SystemTime::now()) {
                Ok(d) => format!("in {}", human_duration(d)),
                Err(_) => "expired".to_string(),
            },
        }
    }

    fn human_duration(d: Duration) -> String {
        let secs = d.as_secs();
        if secs < 90 {
            format!("{secs}s")
        } else if secs < 5400 {
            format!("{}m", secs / 60)
        } else if secs < 172_800 {
            format!("{}h", secs / 3600)
        } else {
            format!("{}d", secs / 86_400)
        }
    }

    fn cmd_status() {
        match status_lines() {
            Ok(lines) => {
                for line in lines {
                    println!("{line}");
                }
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }

    fn cmd_logout(app: Option<&str>) {
        match logout_line(app) {
            Ok(line) => println!("{line}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }
}

pub mod backup {
    //! `pantheon backup`: atomic snapshots of every SQLite database Pantheon
    //! owns, plus restore.
    //!
    //! Snapshots go to `<data_dir>/backups/<UTC-timestamp>/` (overridable with
    //! `--dir`), one file per database plus a `manifest.json`. Each snapshot
    //! is taken with SQLite `VACUUM INTO`, so it is transactionally consistent
    //! even while the gateway or a session is writing.
    //!
    //! Recommended schedule (a scheduled job runs agent task text, so the job
    //! is a task that runs the verb):
    //!   pantheon schedule "run `pantheon backup` in the shell" --every 24h --deliver log

    use std::path::{Path, PathBuf};

    pub fn usage() -> &'static str {
        "usage: pantheon backup [--dir DIR] [--list] [--restore DIR --yes]\n\
         \n\
         Snapshot every SQLite database under the data dir (ledger, memory,\n\
         collaboration, claims, ideas, ...) into backups/<UTC-timestamp>/ via\n\
         atomic VACUUM INTO copies, with a manifest.json per backup.\n\
         \n\
         --dir DIR       backup root (default: <data_dir>/backups)\n\
         --list          list existing backups, newest first\n\
         --restore DIR   copy a backup's databases back over the live ones.\n\
                         Destructive: requires --yes, and stop every Pantheon\n\
                         process first (serve, gateway, sessions).\n\
         \n\
         Recommended schedule:\n\
           pantheon schedule \"run `pantheon backup` in the shell\" --every 24h --deliver log"
    }

    fn utc_stamp() -> String {
        // No chrono in this crate; UTC from the system clock, formatted by hand.
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Days since epoch -> civil date (Howard Hinnant's algorithm).
        let days = (secs / 86400) as i64;
        let z = days + 719468;
        let era = z.div_euclid(146097);
        let doe = z.rem_euclid(146097);
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        let t = secs % 86400;
        let hh = t / 3600;
        let mm = (t % 3600) / 60;
        let ss = t % 60;
        format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
    }

    fn stamp_dir_name() -> String {
        utc_stamp().replace([':', '-'], "").replace('T', "-")
    }

    fn manifest(_backup_dir: &Path, files: &[(String, u64)]) -> String {
        let entries: Vec<String> = files
            .iter()
            .map(|(n, b)| format!("    {{\"name\": \"{n}\", \"bytes\": {b}}}"))
            .collect();
        format!(
            "{{\n  \"created_utc\": \"{}\",\n  \"pantheon_version\": \"{}\",\n  \"files\": [\n{}\n  ]\n}}\n",
            utc_stamp(),
            env!("CARGO_PKG_VERSION"),
            entries.join(",\n")
        )
    }

    fn list_backups(root: &Path) {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
            .map(|r| {
                r.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir() && p.join("manifest.json").is_file())
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort();
        dirs.reverse();
        if dirs.is_empty() {
            println!("no backups in {}", root.display());
            return;
        }
        for d in dirs {
            let name = d.file_name().unwrap_or_default().to_string_lossy();
            let manifest_text =
                std::fs::read_to_string(d.join("manifest.json")).unwrap_or_default();
            let created: String = manifest_text
                .lines()
                .find_map(|l| {
                    let l = l.trim();
                    l.strip_prefix("\"created_utc\":")
                        .map(|v| v.trim().trim_matches(['"', ',', ' ']).to_string())
                })
                .unwrap_or_default();
            let count = manifest_text.matches("\"name\":").count();
            println!("{name}  {created}  {count} databases");
        }
    }

    fn cmd_restore(dir: &str, yes: bool, data_dir: &Path) {
        let src = PathBuf::from(dir);
        let manifest_path = src.join("manifest.json");
        if !manifest_path.is_file() {
            eprintln!("backup: {dir} is not a pantheon backup (no manifest.json)");
            std::process::exit(1);
        }
        if !yes {
            eprintln!("backup: --restore overwrites the live databases.");
            eprintln!("stop every Pantheon process first (serve, gateway, sessions),");
            eprintln!("then re-run with --yes to confirm.");
            std::process::exit(2);
        }
        // Refuse while a live database looks open-for-write by someone else:
        // a best-effort guard, not a lock - the operator stops the processes.
        let manifest_text = std::fs::read_to_string(&manifest_path).unwrap_or_else(|e| {
            eprintln!("backup: cannot read {}: {e}", manifest_path.display());
            std::process::exit(1);
        });
        let manifest: serde_json::Value =
            serde_json::from_str(&manifest_text).unwrap_or_else(|e| {
                eprintln!("backup: cannot parse {}: {e}", manifest_path.display());
                std::process::exit(1);
            });
        let mut names: Vec<String> = Vec::new();
        for f in manifest
            .get("files")
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            if let Some(n) = f.get("name").and_then(|v| v.as_str()) {
                names.push(n.to_string());
            }
        }
        if names.is_empty() {
            eprintln!("backup: manifest lists no databases; nothing to restore");
            std::process::exit(1);
        }
        for name in &names {
            // Stay inside the backup dir: manifest names are plain file names.
            if name.contains('/') || name.contains('\\') || name == "manifest.json" {
                eprintln!("backup: refusing suspicious manifest entry {name:?}");
                std::process::exit(1);
            }
            let from = src.join(name);
            let to = data_dir.join(name);
            if !from.is_file() {
                eprintln!("backup: backup is missing {name}; aborting (nothing restored)");
                std::process::exit(1);
            }
            pantheon_storage::backup::restore_db(&from, &to).unwrap_or_else(|e| {
                eprintln!("backup: {e}");
                std::process::exit(1);
            });
            println!("restored {name}");
        }
        println!("restore complete from {}", src.display());
    }

    pub fn cmd_backup(args: &[String]) {
        if args.iter().any(|a| a == "--help" || a == "-h") {
            println!("{}", usage());
            return;
        }
        let flag_val = |name: &str| {
            let mut it = args.iter().peekable();
            while let Some(a) = it.next() {
                if a == name {
                    return it.next().cloned();
                }
                if let Some(v) = a.strip_prefix(&format!("{name}=")) {
                    return Some(v.to_string());
                }
            }
            None
        };
        let data_dir = crate::terminal::data_dir();
        let root = flag_val("--dir")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("backups"));

        if args.iter().any(|a| a == "--list") {
            list_backups(&root);
            return;
        }
        if let Some(dir) = flag_val("--restore") {
            let yes = args.iter().any(|a| a == "--yes");
            cmd_restore(&dir, yes, &data_dir);
            return;
        }
        if let Some(other) = args.iter().skip(2).find(|a| a.starts_with("--")) {
            eprintln!("backup: unknown flag '{other}'");
            eprintln!("{}", usage());
            std::process::exit(2);
        }

        let dbs = pantheon_storage::backup::discover_dbs(&data_dir);
        if dbs.is_empty() {
            println!("no databases found under {}", data_dir.display());
            return;
        }
        let dest = root.join(stamp_dir_name());
        if let Err(e) = std::fs::create_dir_all(&dest) {
            eprintln!("backup: cannot create {}: {e}", dest.display());
            std::process::exit(1);
        }
        let mut files: Vec<(String, u64)> = Vec::new();
        for src in &dbs {
            let name = src
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let target = dest.join(&name);
            match pantheon_storage::backup::snapshot_db(src, &target) {
                Ok(()) => {
                    let bytes = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
                    println!("snapshotted {name} ({bytes} bytes)");
                    files.push((name, bytes));
                }
                Err(e) => {
                    eprintln!("backup: {e}");
                    std::process::exit(1);
                }
            }
        }
        if let Err(e) = std::fs::write(dest.join("manifest.json"), manifest(&dest, &files)) {
            eprintln!("backup: snapshots taken but manifest failed: {e}");
            std::process::exit(1);
        }
        println!("backup complete: {}", dest.display());
    }
}

pub mod checkpoint {
    //! `/checkpoint`, `/checkpoints`, `/restore` - named durable session snapshots.
    //!
    //! A checkpoint is a `CheckpointCreated { run_id, turn_id, name }` ledger
    //! event: append-only and durable across restarts, following the
    //! `TurnRewound` pattern. `/restore` rewinds to a checkpoint by emitting
    //! `TurnRewound` at the first turn *after* the checkpoint's turn, reusing
    //! the same marker-then-truncate path as double-Esc rewind.
    //!
    //! The pure cores (`effective_turn_ids`, `list_checkpoints`, `plan_restore`)
    //! operate on replayed ledger entries so they are directly testable.

    use pantheon_api::events::Event;
    use pantheon_storage::LedgerEntry;
    use std::sync::Arc;

    use crate::session::{BlockKind, TuiState};
    use pantheon_runtime::session::Session;

    /// One checkpoint as listed by `/checkpoints`.
    ///
    /// `turn_no` is 1-based in the effective (post-rewind) turn order; `0`
    /// means the checkpoint's turn is no longer in the effective history
    /// (it was rewound over) and the checkpoint cannot be restored.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CheckpointInfo {
        pub name: String,
        pub turn_id: String,
        pub turn_no: usize,
    }

    /// A validated restore: what to rewind and where to truncate the view.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RestorePlan {
        pub name: String,
        /// 1-based turn number kept (the checkpoint's turn).
        pub keep_turn_no: usize,
        /// 1-based turn number of the first dropped turn.
        pub drop_from_turn_no: usize,
        /// Turn id to name in the `TurnRewound` marker: the first turn after
        /// the checkpoint. Naming it (rather than the checkpoint turn)
        /// keeps the checkpoint turn itself in the replay.
        pub rewind_target: String,
        pub dropped: usize,
    }

    /// Turn ids of the effective history, in order (1-based by position).
    pub fn effective_turn_ids(entries: &[LedgerEntry]) -> Vec<String> {
        entries
            .iter()
            .filter_map(|e| match &e.event {
                Event::TurnStarted { turn_id, .. } => Some(turn_id.clone()),
                _ => None,
            })
            .collect()
    }

    /// All checkpoints in the effective history, oldest first.
    ///
    /// `turn_no == 0` means the checkpoint's turn is not in the supplied
    /// entries. That happens only for raw (un-replayed) entries: on the
    /// `/checkpoints` path the entries come from `replay()`, which applies
    /// `TurnRewound` by truncating at the rewound turn - so a checkpoint
    /// rewound over vanishes from the list entirely instead of appearing
    /// with `turn_no == 0`. The `0` branch stays as the honest answer for
    /// raw-entry callers (and for `plan_restore`'s error path); it is not
    /// reachable via `/checkpoints` today, and the old doc claiming it was
    /// is corrected here.
    pub fn list_checkpoints(entries: &[LedgerEntry]) -> Vec<CheckpointInfo> {
        let turns = effective_turn_ids(entries);
        entries
            .iter()
            .filter_map(|e| match &e.event {
                Event::CheckpointCreated { name, turn_id, .. } => {
                    let turn_no = turns
                        .iter()
                        .position(|t| t == turn_id)
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    Some(CheckpointInfo {
                        name: name.clone(),
                        turn_id: turn_id.clone(),
                        turn_no,
                    })
                }
                _ => None,
            })
            .collect()
    }

    /// True when a checkpoint with this name already exists in the effective history.
    pub fn checkpoint_exists(entries: &[LedgerEntry], name: &str) -> bool {
        list_checkpoints(entries).iter().any(|c| c.name == name)
    }

    /// Next free auto-name: `cp-1`, `cp-2`, ... (skips names already taken).
    pub fn next_auto_name(entries: &[LedgerEntry]) -> String {
        let mut n = list_checkpoints(entries).len() + 1;
        loop {
            let candidate = format!("cp-{n}");
            if !checkpoint_exists(entries, &candidate) {
                return candidate;
            }
            n += 1;
        }
    }

    /// Validate a restore: resolve the checkpoint, ensure its turn is still in
    /// the effective history and that there is at least one later turn to drop.
    pub fn plan_restore(entries: &[LedgerEntry], name: &str) -> Result<RestorePlan, String> {
        let turns = effective_turn_ids(entries);
        let cp = list_checkpoints(entries)
            .into_iter()
            .find(|c| c.name == name)
            .ok_or_else(|| format!("no checkpoint named '{name}'"))?;
        let idx = turns.iter().position(|t| t == &cp.turn_id).ok_or_else(|| {
            format!("checkpoint '{name}' was rewound over and cannot be restored")
        })?;
        if idx + 1 >= turns.len() {
            return Err(format!("already at checkpoint '{name}'; nothing to rewind"));
        }
        let keep_turn_no = idx + 1;
        Ok(RestorePlan {
            name: name.to_string(),
            keep_turn_no,
            drop_from_turn_no: keep_turn_no + 1,
            rewind_target: turns[idx + 1].clone(),
            dropped: turns.len() - keep_turn_no,
        })
    }

    /// Block index of the Nth (1-based) user message in the live view.
    fn user_message_block(
        blocks: &[crate::session::TranscriptBlock],
        turn_no: usize,
    ) -> Option<usize> {
        let mut n = 0;
        for (i, b) in blocks.iter().enumerate() {
            if matches!(b.kind, BlockKind::UserMessage(_)) {
                n += 1;
                if n == turn_no {
                    return Some(i);
                }
            }
        }
        None
    }

    fn replay_entries(state: &mut TuiState, session: &Arc<Session>) -> Option<Vec<LedgerEntry>> {
        match session.supervisor.replay(&state.session_id) {
            Ok(e) => Some(e),
            Err(err) => {
                state.add_status(format!("ledger replay failed: {err}"));
                None
            }
        }
    }

    /// `/checkpoint [name]` - snapshot the current turn under a durable name.
    pub fn cmd_checkpoint(state: &mut TuiState, session: &Arc<Session>, cmd: &str) {
        if !state.ready || state.active_run.is_some() {
            state.add_status("/checkpoint: wait for the turn to finish".into());
            return;
        }
        let arg = cmd.strip_prefix("/checkpoint").unwrap_or("").trim();
        let entries = match replay_entries(state, session) {
            Some(e) => e,
            None => return,
        };
        let turns = effective_turn_ids(&entries);
        let Some(turn_id) = turns.last().cloned() else {
            state.add_status("/checkpoint: no turn yet to snapshot".into());
            return;
        };
        let name = if arg.is_empty() {
            next_auto_name(&entries)
        } else {
            if arg.contains(char::is_whitespace) || arg.is_empty() {
                state.add_status("usage: /checkpoint [name]  (no spaces in name)".into());
                return;
            }
            arg.to_string()
        };
        if checkpoint_exists(&entries, &name) {
            state.add_status(format!("checkpoint '{name}' already exists"));
            return;
        }
        if let Err(e) = session.supervisor.emit(Event::CheckpointCreated {
            run_id: state.session_id.clone(),
            turn_id,
            name: name.clone(),
        }) {
            state.add_status(format!("/checkpoint: ledger write failed: {e}"));
            return;
        }
        let turn_no = turns.len();
        state.add_status(format!("checkpoint '{name}' saved at turn {turn_no}"));
    }

    /// `/checkpoints` - list checkpoints for this session.
    pub fn cmd_checkpoints(state: &mut TuiState, session: &Arc<Session>) {
        let entries = match replay_entries(state, session) {
            Some(e) => e,
            None => return,
        };
        let cps = list_checkpoints(&entries);
        if cps.is_empty() {
            state.add_status("no checkpoints - /checkpoint [name] to save one".into());
            return;
        }
        state.add_status(format!("checkpoints ({}):", cps.len()));
        for cp in cps {
            if cp.turn_no == 0 {
                state.add_status(format!(
                    "  {} - turn rewound over (cannot restore)",
                    cp.name
                ));
            } else {
                state.add_status(format!("  {} - turn {}", cp.name, cp.turn_no));
            }
        }
    }

    /// `/restore <name>` - rewind the session to a checkpoint.
    pub fn cmd_restore(state: &mut TuiState, session: &Arc<Session>, cmd: &str) {
        if !state.ready || state.active_run.is_some() || state.pending_approval.is_some() {
            state.add_status("/restore: wait for the turn to finish".into());
            return;
        }
        let name = cmd.strip_prefix("/restore").unwrap_or("").trim();
        if name.is_empty() {
            state.add_status("usage: /restore <name>".into());
            return;
        }
        let entries = match replay_entries(state, session) {
            Some(e) => e,
            None => return,
        };
        let plan = match plan_restore(&entries, name) {
            Ok(p) => p,
            Err(e) => {
                state.add_status(format!("/restore: {e}"));
                return;
            }
        };
        // Resolve the view truncation BEFORE emitting the marker: the marker
        // write gates the truncation, and a view that cannot be mapped must
        // not leave the ledger ahead of what the operator sees.
        let Some(drop_block) = user_message_block(&state.blocks, plan.drop_from_turn_no) else {
            state.add_status(format!(
                "/restore: turn {} not found in view; ledger untouched",
                plan.drop_from_turn_no
            ));
            return;
        };
        crate::session::rewind_to_turn(
            state,
            session,
            &plan.rewind_target,
            drop_block,
            plan.dropped as u32,
            &format!("checkpoint '{}' (turn {})", plan.name, plan.keep_turn_no),
        );
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn entry(seq: i64, event: Event) -> LedgerEntry {
            LedgerEntry {
                id: seq,
                run_id: "r".to_string(),
                seq,
                run_seq: seq,
                version: 1,
                ts_ms: seq * 1000,
                event,
            }
        }

        fn turn(turn_id: &str) -> Event {
            Event::TurnStarted {
                run_id: "r".to_string(),
                turn_id: turn_id.to_string(),
            }
        }

        fn checkpoint(name: &str, turn_id: &str) -> Event {
            Event::CheckpointCreated {
                run_id: "r".to_string(),
                turn_id: turn_id.to_string(),
                name: name.to_string(),
            }
        }

        fn history() -> Vec<LedgerEntry> {
            vec![
                entry(1, turn("t1")),
                entry(2, checkpoint("cp-a", "t1")),
                entry(3, turn("t2")),
                entry(4, checkpoint("cp-b", "t2")),
                entry(5, turn("t3")),
            ]
        }

        #[test]
        fn list_checkpoints_numbers_in_effective_order() {
            let cps = list_checkpoints(&history());
            assert_eq!(cps.len(), 2);
            assert_eq!(cps[0].turn_no, 1);
            assert_eq!(cps[1].turn_no, 2);
            assert!(checkpoint_exists(&history(), "cp-a"));
            assert!(!checkpoint_exists(&history(), "cp-z"));
        }

        #[test]
        fn list_checkpoints_zero_for_turn_missing_from_raw_entries() {
            // Raw entries (no replay truncation): a checkpoint whose turn
            // is absent reports turn_no == 0 instead of being dropped.
            let entries = vec![entry(1, checkpoint("cp-orphan", "t9"))];
            let cps = list_checkpoints(&entries);
            assert_eq!(cps.len(), 1);
            assert_eq!(cps[0].turn_no, 0);
        }

        #[test]
        fn plan_restore_keeps_checkpoint_turn_and_drops_later() {
            let plan = plan_restore(&history(), "cp-a").unwrap();
            assert_eq!(plan.keep_turn_no, 1);
            assert_eq!(plan.drop_from_turn_no, 2);
            assert_eq!(plan.rewind_target, "t2");
            assert_eq!(plan.dropped, 2);
        }

        #[test]
        fn plan_restore_rejects_rewound_over_and_at_tip() {
            let orphan = vec![entry(1, checkpoint("cp-orphan", "t9"))];
            assert!(plan_restore(&orphan, "cp-orphan").is_err());
            assert!(plan_restore(&history(), "nope").is_err());
            // cp-b sits on t2 with t3 after it, so restoring it is valid
            // and drops exactly t3 (an earlier draft of this test wrongly
            // claimed cp-b was at the tip).
            let plan = plan_restore(&history(), "cp-b").unwrap();
            assert_eq!(plan.keep_turn_no, 2);
            assert_eq!(plan.drop_from_turn_no, 3);
            assert_eq!(plan.rewind_target, "t3");
            assert_eq!(plan.dropped, 1);
            // A checkpoint on the last turn has nothing later to drop.
            let mut at_tip = history();
            at_tip.push(entry(6, checkpoint("cp-c", "t3")));
            assert!(plan_restore(&at_tip, "cp-c").is_err());
        }
    }
}
