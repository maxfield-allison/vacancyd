//! The credential vacancyd presents to an upstream that authenticates its
//! callers, such as llama.cpp started with `--api-key` or a service behind
//! HTTP Basic authentication.
//!
//! It is configured per upstream and read from a secret file or an
//! environment variable, never from the config file itself and never from
//! the target URL. vacancyd sends it on every request it makes to that
//! upstream: forwarded borrower requests, WebSocket upgrades, and its own
//! observation, completion and unload calls. The resolved value is marked
//! sensitive and has no path to a log line, `/status` or an error message.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use reqwest::header::HeaderValue;
use serde::Deserialize;

/// How the secret is presented in the `Authorization` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    /// `Authorization: Bearer <secret>`, the form llama.cpp's `--api-key`
    /// and most OpenAI-compatible servers expect.
    Bearer,
    /// `Authorization: Basic <base64(username:secret)>` (RFC 7617).
    Basic,
}

/// One upstream's credential, as written in the config file. It holds only
/// where the secret is, so the derived `Debug` prints nothing secret.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamCredential {
    pub scheme: Scheme,
    /// The user name for `basic`. Not a secret; not allowed for `bearer`.
    #[serde(default)]
    pub username: Option<String>,
    /// Read the secret from this environment variable.
    #[serde(default)]
    pub value_env: Option<String>,
    /// Read the secret from this file, resolved relative to the config
    /// file. Trailing whitespace is trimmed, so a file written by `echo`
    /// works.
    #[serde(default)]
    pub value_file: Option<PathBuf>,
}

impl UpstreamCredential {
    /// The rules that need no secret: checked with the rest of the
    /// configuration.
    pub fn validate(&self, upstream: &str) -> Result<()> {
        let sources =
            usize::from(self.value_env.is_some()) + usize::from(self.value_file.is_some());
        if sources != 1 {
            bail!("upstream '{upstream}': credential needs exactly one of value_env, value_file (found {sources})");
        }
        if self
            .value_env
            .as_deref()
            .is_some_and(|name| name.trim().is_empty())
        {
            bail!("upstream '{upstream}': credential value_env must not be empty");
        }
        if self
            .value_file
            .as_deref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            bail!("upstream '{upstream}': credential value_file must not be empty");
        }
        match (self.scheme, self.username.as_deref()) {
            (Scheme::Bearer, Some(_)) => {
                bail!(
                    "upstream '{upstream}': credential username applies only to scheme = \"basic\""
                )
            }
            (Scheme::Basic, None) => {
                bail!("upstream '{upstream}': credential scheme = \"basic\" needs a username")
            }
            (Scheme::Basic, Some(name))
                if name.is_empty() || name.contains(':') || name.chars().any(char::is_control) =>
            {
                // RFC 7617 section 2: the user-id cannot contain a colon.
                bail!("upstream '{upstream}': credential username must be non-empty, without ':' or control characters")
            }
            _ => Ok(()),
        }
    }

    /// Read the secret and build the `Authorization` value, marked sensitive
    /// so `Debug` of a header map or client never prints it. Error messages
    /// name where the secret was looked for, never what was found.
    pub fn resolve(&self, upstream: &str) -> Result<HeaderValue> {
        self.validate(upstream)?;
        let secret = if let Some(var) = &self.value_env {
            std::env::var(var).map_err(|_| {
                anyhow::anyhow!("upstream '{upstream}': credential environment variable {var} is not set or not UTF-8")
            })?
        } else {
            let path = self.value_file.as_ref().expect("validated above");
            std::fs::read_to_string(path).with_context(|| {
                format!(
                    "upstream '{upstream}': reading credential {}",
                    path.display()
                )
            })?
        };
        let secret = secret.trim_end();
        if secret.is_empty() {
            bail!("upstream '{upstream}': credential resolved to an empty value");
        }
        if secret.chars().any(char::is_control) {
            bail!("upstream '{upstream}': credential contains a control character");
        }
        let text = match self.scheme {
            Scheme::Bearer => {
                // RFC 6750 section 2.1: a bearer token is printable ASCII.
                if !secret.chars().all(|c| c.is_ascii_graphic()) {
                    bail!("upstream '{upstream}': a bearer credential must be printable ASCII without whitespace");
                }
                format!("Bearer {secret}")
            }
            Scheme::Basic => {
                let username = self.username.as_deref().expect("validated above");
                format!(
                    "Basic {}",
                    base64(format!("{username}:{secret}").as_bytes())
                )
            }
        };
        // Both forms are printable ASCII by now, so this cannot fail; the
        // error type would carry no part of the value if it did.
        let mut value = HeaderValue::from_str(&text).map_err(|_| {
            anyhow::anyhow!("upstream '{upstream}': credential is not a valid header value")
        })?;
        value.set_sensitive(true);
        Ok(value)
    }
}

/// Standard base64 with padding (RFC 4648 section 4), for Basic credentials.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let bytes = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let group = u32::from(bytes[0]) << 16 | u32::from(bytes[1]) << 8 | u32::from(bytes[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(char::from(
                    ALPHABET[(group >> (18 - 6 * index) & 0x3f) as usize],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(toml: &str) -> UpstreamCredential {
        toml::from_str(toml).unwrap()
    }

    /// A secret file removed when the test ends.
    struct SecretFile(PathBuf);

    impl Drop for SecretFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A credential of `toml` reading its secret from a new file holding
    /// `contents`.
    fn from_file(toml: &str, contents: &str) -> (UpstreamCredential, SecretFile) {
        let path = std::env::temp_dir().join(format!(
            "vacancyd-upstream-credential-{}",
            crate::setup::nonce().unwrap()
        ));
        std::fs::write(&path, contents).unwrap();
        let mut c = credential(&format!("{toml}\nvalue_file = \"x\""));
        c.value_file = Some(path.clone());
        (c, SecretFile(path))
    }

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected, "{input}");
        }
        assert_eq!(base64(&[0xfb, 0xff, 0xfe]), "+//+");
    }

    #[test]
    fn a_bearer_file_becomes_a_sensitive_header() {
        let (c, _file) = from_file("scheme = \"bearer\"", "fake-upstream-secret\n");
        let value = c.resolve("llama").unwrap();
        assert_eq!(value.to_str().unwrap(), "Bearer fake-upstream-secret");
        assert!(value.is_sensitive());
        assert!(!format!("{value:?}").contains("fake-upstream-secret"));
    }

    #[test]
    fn basic_follows_rfc_7617() {
        let (c, _file) = from_file("scheme = \"basic\"\nusername = \"Aladdin\"", "open sesame");
        assert_eq!(
            c.resolve("svc").unwrap().to_str().unwrap(),
            "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
        // RFC 7617 passwords are UTF-8; base64 keeps the header ASCII.
        let (c, _file) = from_file("scheme = \"basic\"\nusername = \"u\"", "pässword");
        assert_eq!(
            c.resolve("svc").unwrap().to_str().unwrap(),
            format!("Basic {}", base64("u:pässword".as_bytes()))
        );
    }

    #[test]
    fn configuration_mistakes_are_refused() {
        for (toml, expected) in [
            ("scheme = \"bearer\"", "exactly one of"),
            (
                "scheme = \"bearer\"\nvalue_env = \"A\"\nvalue_file = \"b\"",
                "exactly one of",
            ),
            (
                "scheme = \"bearer\"\nvalue_env = \" \"",
                "must not be empty",
            ),
            (
                "scheme = \"bearer\"\nvalue_file = \"\"",
                "must not be empty",
            ),
            (
                "scheme = \"bearer\"\nusername = \"u\"\nvalue_env = \"A\"",
                "only to scheme",
            ),
            ("scheme = \"basic\"\nvalue_env = \"A\"", "needs a username"),
            (
                "scheme = \"basic\"\nusername = \"a:b\"\nvalue_env = \"A\"",
                "without ':'",
            ),
            (
                "scheme = \"basic\"\nusername = \"\"\nvalue_env = \"A\"",
                "non-empty",
            ),
        ] {
            let error = credential(toml).validate("u1").unwrap_err().to_string();
            assert!(error.contains(expected), "{toml}: {error}");
            assert!(error.contains("'u1'"), "{error}");
        }
        assert!(
            toml::from_str::<UpstreamCredential>("scheme = \"digest\"\nvalue_env = \"A\"").is_err()
        );
        assert!(
            toml::from_str::<UpstreamCredential>("scheme = \"bearer\"\nvalue = \"inline\"")
                .is_err()
        );
    }

    #[test]
    fn unusable_secrets_are_refused_without_being_echoed() {
        for (contents, expected) in [
            ("", "empty value"),
            ("  \n", "empty value"),
            ("fake secret", "without whitespace"),
            ("fake\tsecret", "control character"),
            ("fake\u{7}secret", "control character"),
            ("fake-sécret", "printable ASCII"),
        ] {
            let (c, _file) = from_file("scheme = \"bearer\"", contents);
            let error = format!("{:#}", c.resolve("u1").unwrap_err());
            assert!(error.contains(expected), "{contents:?}: {error}");
            assert!(!error.contains("fake"), "{error}");
        }
        let mut c = credential("scheme = \"bearer\"\nvalue_file = \"x\"");
        c.value_file = Some(std::env::temp_dir().join("vacancyd-no-such-credential-file"));
        let error = format!("{:#}", c.resolve("u1").unwrap_err());
        assert!(error.contains("reading credential"), "{error}");
        let c = credential(
            "scheme = \"bearer\"\nvalue_env = \"VACANCYD_TEST_UNSET_UPSTREAM_CREDENTIAL\"",
        );
        assert!(c
            .resolve("u1")
            .unwrap_err()
            .to_string()
            .contains("is not set"));
    }
}
