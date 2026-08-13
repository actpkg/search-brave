//! The credential this component searches with, and how to read it.
//!
//! One credential, one field, in this component's own namespace.
//! `ACT-CONSTANTS.md` §8.2 registers field *types*, not field *names* — whoever
//! stores a credential names its fields, and the component reading them is the
//! party that asked for those names. A component may not mint a name in the
//! `std:` namespace at all (`act-build pack` refuses one), so `brave:api-key`
//! is both the correct name and the only kind of name that can be declared in
//! `act.toml` for `act login` to prompt for.
//!
//! Pure by design: the `get-secret` call lives in `lib.rs`, because the WIT
//! bindings exist only inside the `#[act_component]` module. That split is what
//! lets everything below be tested on the host target — the messages an
//! operator has to act on are exactly the part worth pinning.

use act_sdk::credentials::Secret;
use act_sdk::prelude::*;

/// The credential key a session uses when its args do not name one. Declared
/// in `act.toml` under `[[std.credentials]]`, so `act login <ref>` prompts for
/// it with zero arguments.
pub const DEFAULT_CREDENTIAL_KEY: &str = "default";

/// The one field this component reads: Brave's `X-Subscription-Token`.
pub const FIELD_API_KEY: &str = "brave:api-key";

/// The kind for "the credential this component needs is not usable"
/// (`ACT-CONSTANTS.md` §9). Spelled out because act-types 0.14 exports no
/// constant for it — its `constants` module has `ERR_NOT_FOUND`,
/// `ERR_INVALID_ARGS`, `ERR_TIMEOUT`, `ERR_CAPABILITY_DENIED`, `ERR_INTERNAL`
/// and `ERR_SESSION_NOT_FOUND` and no more. **Delete this and import it once
/// the SDK exports one.**
pub const ERR_CREDENTIAL_REQUIRED: &str = "std:credential-required";

/// The longest a credential key may be. A lookup name, not a sentence: long
/// enough for any key an operator would type, short enough that it cannot
/// become a paragraph inside the consent prompt a *human* reads.
pub const MAX_CREDENTIAL_KEY: usize = 64;

/// The leading credential-key-shaped run of `key`: letters, digits, `-`, `_`
/// and `.`, up to [`MAX_CREDENTIAL_KEY`] characters.
///
/// Returns the *prefix* so a refusal can name what was asked for without
/// echoing whatever followed it.
pub fn key_prefix(key: &str) -> &str {
    let end = key
        .as_bytes()
        .iter()
        .take(MAX_CREDENTIAL_KEY)
        .position(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')))
        .unwrap_or_else(|| key.len().min(MAX_CREDENTIAL_KEY));
    &key[..end]
}

/// Refuse a `credential_key` that is not a name.
///
/// `credential_key` is agent-authored text that the host pastes into the
/// question a **human** answers when deciding to release a credential
/// (`act-cli/crates/act-runtime/src/credentials.rs`, where only `hint` is
/// sanitised on the stated reasoning that everything else is host-derived —
/// here it is not). The hazard is not newline forgery, which the prompter
/// escapes: it is that `default (approved by your administrator)` renders in
/// that question exactly as written and the human cannot tell which half the
/// machine wrote. A key is a lookup name, so a bounded token ends the question.
pub fn validate_key(key: &str) -> ActResult<()> {
    let head = key_prefix(key);
    if head.is_empty() || head.len() != key.len() {
        return Err(ActError::invalid_args(format!(
            "credential_key must be a name: 1–{MAX_CREDENTIAL_KEY} characters of letters, \
             digits, '-', '_' or '.', and '{head}…' is not one. It is a lookup key in this \
             component's credential profile, not a sentence."
        )));
    }
    Ok(())
}

/// Read the subscription token out of a fetched secret.
///
/// There is no shape accessor — a `std:string` field's value *is* a CBOR
/// string, so it is read by name with `field_str`.
pub fn from_secret(secret: &Secret, key: &str) -> ActResult<String> {
    secret
        .field_str(FIELD_API_KEY)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| missing_field(key))
}

/// The command that provisions this component's credential, ready to paste.
///
/// The `--field` name is part of it because `act secret set` has no default
/// field set: a credential *is* its named fields.
pub fn provision_command(key: &str) -> String {
    format!(
        "act secret set <component-ref> --key {key} --field {FIELD_API_KEY} --fields-stdin\n  \
         {{\"{FIELD_API_KEY}\": \"<Web Search subscription token>\"}}"
    )
}

/// `not-found` and `denied` collapse into this one message on purpose: the
/// host decides `denied` before it consults the store (ACT-AUTH §1.1.7), so
/// distinguishing them would invent a difference the host refuses to disclose
/// — and would turn the pair into a way to probe a profile for keys. So it
/// names both causes and asserts neither.
///
/// `known` is the store's key inventory, best effort: a policy that denies the
/// store denies the listing too, and then the message simply carries none.
/// Keys are not credential material — `list-secrets` exists to hand them to
/// the agent.
pub fn credential_missing_message(key: &str, known: &[String]) -> String {
    let mut msg = format!(
        "No usable credential under key '{key}'. Either it is not set, or policy denies \
         act:credentials for this component. Get a Web Search token at \
         api-dashboard.search.brave.com and store it with:\n  {}",
        provision_command(key)
    );
    if !known.is_empty() {
        msg.push_str(&format!(
            "\nThis component's profile has: {}",
            known.join(", ")
        ));
    }
    msg
}

/// The credential exists but carries no [`FIELD_API_KEY`].
///
/// `std:credential-required` rather than `std:invalid-args`: nothing about the
/// *call* was wrong, and the fix is the provisioning command a host surfaces
/// for exactly this kind (`ACT-CONSTANTS.md` §9).
pub fn missing_field(key: &str) -> ActError {
    ActError::new(
        ERR_CREDENTIAL_REQUIRED,
        format!(
            "The credential under key '{key}' carries no usable {FIELD_API_KEY} field. \
             search-brave authenticates with a Brave subscription token only. Store one \
             with:\n  {}",
            provision_command(key)
        ),
    )
}

/// Brave refused the stored token. The fix is in the store, not in the call,
/// so this is `std:credential-required` and not `std:invalid-args` — the old
/// message told the operator to check `api_key` in the session args, a place
/// the token no longer lives.
///
/// `detail` is Brave's own `detail` string (or a truncated body). It describes
/// the *refusal*, never the token: nothing here echoes credential material.
pub fn token_rejected(key: &str, detail: &str) -> ActError {
    ActError::new(
        ERR_CREDENTIAL_REQUIRED,
        format!(
            "Brave Search rejected the subscription token stored under key '{key}'.{detail} \
             It must be a **Web Search** token from api-dashboard.search.brave.com — a token \
             for another Brave product is refused the same way. Replace it with:\n  {}",
            provision_command(key)
        ),
    )
}

/// What every later call in a session is told once Brave has refused its
/// token. See `SessionState::credential_rejected` in `lib.rs` for why the
/// session is poisoned rather than retried.
pub const CREDENTIAL_REJECTED: &str = "Brave rejected this session's subscription token the first time it was used, and no \
     call in this session can change that: the credential is looked up under the key this \
     session was opened with. Retrying would ask the credential store again — and a human, \
     if the store prompts — on every call, so search-brave stops here. Close this session \
     and open a new one once the stored token is fixed; the first call's error says what \
     Brave reported.";

/// The store rejected the session id the component passed it. Nothing the
/// agent supplied is at fault, so nothing it supplied is quoted.
pub const STORE_SESSION_UNKNOWN: &str =
    "the credential store does not recognise this session; open a new one";

/// The store could not answer. `msg` is host-authored, and ACT-AUTH §1.1.7
/// requires it to be free of credential material — including material a
/// backend error quotes back — precisely so it can be passed on like this.
pub fn store_unavailable(msg: &str) -> ActError {
    ActError::internal(format!("the credential store is unavailable: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciborium::Value;

    fn secret(pairs: &[(&str, &str)]) -> Secret {
        Secret {
            kind: "std:fields".into(),
            fields: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), Value::Text((*v).to_string())))
                .collect(),
        }
    }

    #[test]
    fn the_token_is_read_by_name_out_of_the_field_map() {
        let token = from_secret(&secret(&[(FIELD_API_KEY, "BSA-token")]), "default")
            .expect("a usable credential");
        assert_eq!(token, "BSA-token");
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_rather_than_sent() {
        // A token pasted out of a dashboard often carries a trailing newline,
        // and Brave answers that with the same 422 as a wrong token — a
        // refusal whose message would send the operator looking for the wrong
        // problem entirely.
        let token =
            from_secret(&secret(&[(FIELD_API_KEY, " BSA-token\n")]), "default").expect("usable");
        assert_eq!(token, "BSA-token");
    }

    #[test]
    fn a_blank_field_is_not_a_credential() {
        let err = from_secret(&secret(&[(FIELD_API_KEY, "   ")]), "default")
            .expect_err("an empty token would 422 later, further from the cause");
        assert_eq!(err.kind, ERR_CREDENTIAL_REQUIRED);
    }

    #[test]
    fn a_credential_without_the_field_names_the_field_and_the_command() {
        // The near-miss this catches: a token stored under some other name
        // (`api_key`, `token`) reads as "no credential" unless the message
        // says which name this component actually looks for.
        let err = from_secret(&secret(&[("brave:token", "BSA-token")]), "prod")
            .expect_err("a field this component does not read is not its credential");
        assert_eq!(err.kind, ERR_CREDENTIAL_REQUIRED);
        assert!(err.message.contains(FIELD_API_KEY), "got {}", err.message);
        assert!(err.message.contains("--key prod"), "got {}", err.message);
    }

    #[test]
    fn a_non_string_field_is_not_a_token() {
        // The whole reason values cross as CBOR rather than as strings: an
        // integer field must not silently render as text.
        let s = Secret {
            kind: "std:fields".into(),
            fields: [(FIELD_API_KEY.to_string(), Value::Integer(7.into()))]
                .into_iter()
                .collect(),
        };
        assert!(from_secret(&s, "default").is_err());
    }

    #[test]
    fn every_refusal_hands_back_a_runnable_command() {
        // The property that makes these errors worth their length: an
        // operator can paste the message's second line and be done.
        let messages = [
            missing_field("default").message,
            token_rejected("default", " Subscription token invalid.").message,
            credential_missing_message("default", &[]),
        ];
        for m in messages {
            assert!(
                m.contains("act secret set <component-ref> --key default --field brave:api-key --fields-stdin"),
                "not runnable as printed: {m}"
            );
        }
    }

    #[test]
    fn the_missing_credential_message_lists_the_keys_the_profile_does_have() {
        let msg = credential_missing_message("staging", &["default".into(), "prod".into()]);
        assert!(msg.contains("default, prod"), "got {msg}");
    }

    #[test]
    fn no_message_is_ever_built_from_the_secret() {
        // A sweep, not an eyeball. `from_secret` is the only function here
        // that sees a `Secret`, and it *returns* the token rather than
        // printing it; every message is built from a key, a Brave-authored
        // detail and constants. This is what keeps that true as messages get
        // edited — and it covers `Debug` too, which is where credential
        // material escapes by accident.
        let sentinel = "BSA-sentinel-token";
        let s = secret(&[(FIELD_API_KEY, sentinel)]);
        assert_eq!(from_secret(&s, "default").expect("usable"), sentinel);

        let printed = [
            missing_field("default").message,
            token_rejected("default", " Subscription token invalid.").message,
            credential_missing_message("default", &["default".into()]),
            store_unavailable("backend down").message,
            CREDENTIAL_REJECTED.to_string(),
            STORE_SESSION_UNKNOWN.to_string(),
            format!("{s:?}"),
        ];
        for m in printed {
            assert!(!m.contains(sentinel), "leaked credential material: {m}");
        }
    }

    #[test]
    fn a_key_that_is_a_name_is_accepted() {
        for key in [DEFAULT_CREDENTIAL_KEY, "prod", "brave-2", "team.eu_west"] {
            assert!(validate_key(key).is_ok(), "{key} is a name");
        }
    }

    #[test]
    fn a_key_that_is_a_sentence_is_refused_and_only_its_prefix_echoed() {
        // The attack the bound exists for: the key is pasted verbatim into the
        // consent question a human reads, so `default (approved by your
        // administrator)` must not be a key.
        let err = validate_key("default (approved by your administrator)")
            .expect_err("a sentence is not a lookup key");
        assert_eq!(err.kind, act_sdk::constants::ERR_INVALID_ARGS);
        assert!(err.message.contains("'default…'"), "got {}", err.message);
        assert!(
            !err.message.contains("administrator"),
            "the refusal echoed the rest of the key: {}",
            err.message
        );
    }

    #[test]
    fn an_empty_key_is_refused() {
        assert!(validate_key("").is_err());
    }

    #[test]
    fn a_key_longer_than_the_bound_is_refused() {
        assert!(validate_key(&"a".repeat(MAX_CREDENTIAL_KEY + 1)).is_err());
        assert!(validate_key(&"a".repeat(MAX_CREDENTIAL_KEY)).is_ok());
    }
}
