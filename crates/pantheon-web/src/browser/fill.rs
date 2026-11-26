//! Login-field detection for `browser_fill_login`.
//!
//! Pure functions over a page snapshot: no backend calls, no secrets. Two
//! snapshot shapes normalized because backends disagree:
//! * CDP-style: `{"version": N, "elements": [{"ref","role","name"}, ...]}`
//! * GSD/camofox-style: `{"version": N, "refs": {"e0":
//!   {"ref","role","name","type"?}, ...}}` (the `@vN:eM` value is the input
//!   spelling for `fill-ref`; constructed from the key when a backend omits it)
//!
//! Rules (documented because the model only sees the outcome):
//! * A password field is a text input with `type` `password`, or whose
//!   accessible name mentions a password.
//! * A username field is a text input whose name mentions
//!   username/email/login/etc, else the nearest text input *before* the
//!   password field (unlabeled inputs are common on login forms).
//! * More than one password field is ambiguous: error rather than guess which
//!   form the user meant.
//!
//! Name matching ignores buttons and links ("Show password", "Forgot
//! password?"): only text inputs are candidates.

use serde_json::Value;

/// accessible-name fragments marking a password field. multilingual because
/// real login forms are.
const PASSWORD_HINTS: &[&str] = &[
    "password",
    "passwort",
    "mot de passe",
    "contraseña",
    "contrasena",
    "senha",
    "wachtwoord",
    "lösenord",
    "パスワード",
    "密码",
];

/// accessible-name fragments marking a username field
const USERNAME_HINTS: &[&str] = &[
    "username",
    "user name",
    "email",
    "e-mail",
    "login",
    "account",
    "phone",
    "mobile",
    "identifier",
    "nom d'utilisateur",
    "benutzername",
    "usuario",
    "usuário",
    "gebruikersnaam",
    "ユーザー",
    "用户名",
];

/// one detected login field: ref to fill plus a display-safe label (the
/// field's accessible name, never a secret)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginField {
    /// snapshot ref (`@v2:e3`) for `fill-ref`
    pub r: String,
    /// accessible name for the tool result ("Email"), never a secret
    pub label: String,
}

/// detected login fields. either may be absent (two-step flows show one at
/// a time); both absent is [`FillError::NoFields`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LoginFields {
    pub username: Option<LoginField>,
    pub password: Option<LoginField>,
}

/// why field detection failed
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FillError {
    /// no username-like or password-like fields on the page
    NoFields,
    /// more than one password field; don't guess which form the user meant
    AmbiguousPasswordFields(usize),
}

/// a normalized snapshot element: the fields detection needs
#[derive(Debug, Clone)]
struct Element {
    r: String,
    role: String,
    name: String,
    input_type: String,
}

/// normalize either snapshot shape into document order. elements without a
/// usable ref are dropped (nothing to fill).
fn normalize(snapshot: &Value) -> Vec<Element> {
    let mut out = Vec::new();
    let push = |out: &mut Vec<Element>, r: &str, role: &str, name: &str, input_type: &str| {
        if r.is_empty() {
            return;
        }
        out.push(Element {
            r: r.to_string(),
            role: role.to_string(),
            name: name.to_string(),
            input_type: input_type.to_string(),
        });
    };
    // CDP-style: {"elements": [{"ref","role","name"}]}
    if let Some(els) = snapshot.get("elements").and_then(Value::as_array) {
        for el in els {
            push(
                &mut out,
                el.get("ref").and_then(Value::as_str).unwrap_or(""),
                el.get("role").and_then(Value::as_str).unwrap_or(""),
                el.get("name").and_then(Value::as_str).unwrap_or(""),
                el.get("type").and_then(Value::as_str).unwrap_or(""),
            );
        }
        return out;
    }
    // GSD/camofox-style: {"version": N, "refs": {"e0": {...}}}
    if let Some(refs) = snapshot.get("refs").and_then(Value::as_object) {
        // no fabricated version: a snapshot without `version` must not yield
        // `@v0:eM` refs, which may not match the backend's real version.
        // fall back to the bare key
        let version = snapshot.get("version").and_then(Value::as_u64);
        // sort e0, e1, ... e10 numerically for document order
        let mut keys: Vec<&String> = refs.keys().collect();
        keys.sort_by_key(|k| {
            k.trim_start_matches('e')
                .parse::<usize>()
                .unwrap_or(usize::MAX)
        });
        for k in keys {
            let el = &refs[k];
            let r = el
                .get("ref")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| match version {
                    Some(v) => format!("@v{v}:{k}"),
                    None => k.to_string(),
                });
            push(
                &mut out,
                &r,
                el.get("role").and_then(Value::as_str).unwrap_or(""),
                el.get("name").and_then(Value::as_str).unwrap_or(""),
                el.get("type").and_then(Value::as_str).unwrap_or(""),
            );
        }
    }
    out
}

/// true for elements that can hold typed login text. explicit input types are
/// the most reliable signal across backends; without one (CDP AX tree) the
/// `textbox` role is the marker. excluded types are never login fields, even
/// when mislabeled.
fn is_text_input(role: &str, input_type: &str) -> bool {
    match input_type.to_ascii_lowercase().as_str() {
        "text" | "email" | "tel" | "password" | "url" => true,
        "hidden" | "checkbox" | "radio" | "submit" | "button" | "reset" | "file" | "range"
        | "color" | "date" | "time" | "datetime-local" | "month" | "week" | "number" | "search" => {
            false
        }
        _ => role.eq_ignore_ascii_case("textbox"),
    }
}

fn name_has(name: &str, hints: &[&str]) -> bool {
    let lower = name.to_ascii_lowercase();
    hints.iter().any(|h| lower.contains(h))
}

/// locate username/password fields in a snapshot. never sees a secret: works
/// only on refs, roles, and accessible names.
pub fn find_login_fields(snapshot: &Value) -> Result<LoginFields, FillError> {
    let els = normalize(snapshot);
    let text: Vec<usize> = els
        .iter()
        .enumerate()
        .filter(|(_, e)| is_text_input(&e.role, &e.input_type))
        .map(|(i, _)| i)
        .collect();

    let is_password = |i: usize| {
        els[i].input_type.eq_ignore_ascii_case("password") || name_has(&els[i].name, PASSWORD_HINTS)
    };
    let pw: Vec<usize> = text.iter().copied().filter(|&i| is_password(i)).collect();
    if pw.len() > 1 {
        return Err(FillError::AmbiguousPasswordFields(pw.len()));
    }
    let pw_idx = pw.first().copied();

    let field = |i: usize| LoginField {
        r: els[i].r.clone(),
        label: {
            let name = els[i].name.trim();
            if name.is_empty() {
                els[i].role.clone()
            } else {
                name.to_string()
            }
        },
    };

    let is_username = |i: usize| Some(i) != pw_idx && name_has(&els[i].name, USERNAME_HINTS);
    let mut un: Vec<usize> = text.iter().copied().filter(|&i| is_username(i)).collect();
    // nearest to the password field wins among several candidates; without a
    // password field, document order wins
    if un.len() > 1 {
        if let Some(p) = pw_idx {
            un.sort_by_key(|&i| i.abs_diff(p));
        }
    }
    let mut username = un.first().copied().map(field);

    // fallback: nearest text input *before* the password field — unlabeled
    // inputs are common on real login forms
    if username.is_none() {
        if let Some(p) = pw_idx {
            username = text
                .iter()
                .copied()
                .filter(|&i| i < p && Some(i) != pw_idx)
                .max_by_key(|&i| i)
                .map(field);
        }
    }

    let password = pw_idx.map(field);
    if username.is_none() && password.is_none() {
        return Err(FillError::NoFields);
    }
    Ok(LoginFields { username, password })
}

/// true when `site` (a vault login's stored site: bare host or full URL)
/// matches `host` (current page host): exact match, or page host is a
/// subdomain of the stored site. one direction only: matching when the *stored*
/// site is a subdomain of the page host would fill a login saved for
/// `evil.github.com` on `github.com` itself, widening the vault entry's scope
/// to a parent domain the user never saved it for. deliberately *not* a
/// substring match either: `evilgithub.com` must never match a `github.com`
/// login.
pub fn host_matches_site(site: &str, host: &str) -> bool {
    let s = super::tools::host_of(site).to_ascii_lowercase();
    let h = host.trim().to_ascii_lowercase();
    if s.is_empty() || h.is_empty() {
        return false;
    }
    s == h || h.ends_with(&format!(".{s}"))
}

#[cfg(test)]
mod host_match_tests {
    use super::host_matches_site;

    #[test]
    fn exact_and_page_subdomain_match() {
        assert!(host_matches_site("github.com", "github.com"));
        assert!(host_matches_site(
            "https://github.com/login",
            "gist.github.com"
        ));
        assert!(host_matches_site("GITHUB.COM", "github.com"));
    }

    #[test]
    fn stored_subdomain_does_not_match_parent() {
        // one direction only: a login saved for a subdomain must not fill
        // on the parent domain
        assert!(!host_matches_site("evil.github.com", "github.com"));
        assert!(!host_matches_site("evil.github.com", "other.github.com"));
    }

    #[test]
    fn suffix_lookalikes_never_match() {
        assert!(!host_matches_site("github.com", "evilgithub.com"));
        assert!(!host_matches_site("github.com", "github.com.evil.com"));
    }
}
