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
            "Esc cancels. The value is stored in the secrets broker, never in config.".to_string(),
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
