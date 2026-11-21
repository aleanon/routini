//! HTTP Basic authentication (nginx `auth_basic` / `auth_basic_user_file`).
//!
//! Credentials are `user:password`, or `user:<bcrypt hash>` as `htpasswd -nB user` prints it,
//! which keeps the password out of the config file. bcrypt is slow on purpose (tens of
//! milliseconds), so:
//! - hashes are checked on tokio's blocking pool, at most one per CPU at a time, never on a
//!   proxy worker where it would stall every other request on that thread
//! - a successful check is remembered per `Authorization` value, so a client pays for it once
//!   rather than on every request. The memory belongs to the route config: a reload that changes
//!   the credentials starts from scratch

use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, LazyLock},
};

use base64::prelude::{BASE64_STANDARD, Engine};
use quick_cache::sync::Cache;
use tokio::sync::Semaphore;

/// How many verified `Authorization` values each route remembers.
const VERIFIED_CAPACITY: usize = 4096;

/// Bounds concurrent bcrypt checks so a flood of wrong passwords can't take every CPU.
static BCRYPT_PERMITS: LazyLock<Semaphore> =
    LazyLock::new(|| Semaphore::new(std::thread::available_parallelism().map_or(1, usize::from)));

enum Secret {
    Plain(String),
    Bcrypt(String),
}

/// The accepted users of a route. Cheap to clone, clones share the verified memory.
#[derive(Clone)]
pub struct BasicAuth {
    users: Arc<HashMap<String, Secret>>,
    /// `Authorization` values whose bcrypt check passed.
    verified: Arc<Cache<String, ()>>,
}

impl Default for BasicAuth {
    fn default() -> Self {
        Self {
            users: Arc::default(),
            verified: Arc::new(Cache::new(VERIFIED_CAPACITY)),
        }
    }
}

/// Lists the user names only, never the secrets.
impl fmt::Debug for BasicAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut users: Vec<_> = self.users.keys().collect();
        users.sort();
        f.debug_struct("BasicAuth").field("users", &users).finish()
    }
}

impl BasicAuth {
    /// Parse `user:password` / `user:<bcrypt hash>` entries.
    pub fn new(entries: &[String]) -> Result<Self, String> {
        let mut users = HashMap::new();
        for entry in entries {
            let Some((user, secret)) = entry.split_once(':') else {
                return Err(
                    "basic_auth entries must be 'user:password' or 'user:<bcrypt hash>'"
                        .to_string(),
                );
            };
            if user.is_empty() {
                return Err("basic_auth entry with an empty user name".to_string());
            }
            let secret = if secret.starts_with("$2") {
                secret.parse::<bcrypt::HashParts>().map_err(|err| {
                    format!("basic_auth user '{user}': invalid bcrypt hash: {err}")
                })?;
                Secret::Bcrypt(secret.to_string())
            } else if ["$apr1$", "$1$", "$5$", "$6$", "{SHA}"]
                .iter()
                .any(|prefix| secret.starts_with(prefix))
            {
                return Err(format!(
                    "basic_auth user '{user}': only bcrypt hashes are supported, create one with \
                     `htpasswd -nB {user}`"
                ));
            } else {
                Secret::Plain(secret.to_string())
            };
            if users.insert(user.to_string(), secret).is_some() {
                return Err(format!("basic_auth user '{user}' is listed twice"));
            }
        }
        Ok(Self {
            users: Arc::new(users),
            ..Default::default()
        })
    }

    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }

    /// Whether an `Authorization` header value carries valid credentials.
    pub async fn verify(&self, authorization: Option<&str>) -> bool {
        let Some(token) = authorization
            .and_then(|h| h.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("basic"))
            .map(|(_, token)| token.trim())
        else {
            return false;
        };
        if self.verified.get(token).is_some() {
            return true;
        }
        let Some(decoded) = BASE64_STANDARD
            .decode(token)
            .ok()
            .and_then(|d| String::from_utf8(d).ok())
        else {
            return false;
        };
        let Some((user, password)) = decoded.split_once(':') else {
            return false;
        };
        match self.users.get(user) {
            None => false,
            Some(Secret::Plain(expected)) => {
                constant_time_eq(expected.as_bytes(), password.as_bytes())
            }
            Some(Secret::Bcrypt(hash)) => {
                let Ok(_permit) = BCRYPT_PERMITS.acquire().await else {
                    return false;
                };
                let (password, hash) = (password.to_string(), hash.clone());
                let ok = tokio::task::spawn_blocking(move || {
                    bcrypt::verify(password, &hash).unwrap_or(false)
                })
                .await
                .unwrap_or(false);
                if ok {
                    self.verified.insert(token.to_string(), ());
                }
                ok
            }
        }
    }
}

/// Compare without exiting at the first differing byte, so response timing doesn't reveal how
/// much of a password matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(credentials: &str) -> String {
        format!("Basic {}", BASE64_STANDARD.encode(credentials))
    }

    #[tokio::test]
    async fn plain_and_bcrypt_credentials() {
        let hash = bcrypt::hash("s3cret", 4).unwrap();
        let auth = BasicAuth::new(&["alice:pass".into(), format!("bob:{hash}")]).unwrap();

        assert!(auth.verify(Some(&header("alice:pass"))).await);
        assert!(auth.verify(Some(&header("bob:s3cret"))).await);
        // Remembered, and still right the second time
        assert!(auth.verify(Some(&header("bob:s3cret"))).await);
        assert!(
            auth.verify(Some(&header("bob:s3cret").replace("Basic", "basic")))
                .await
        );

        for bad in [
            header("bob:wrong"),
            header("alice:pas"),
            header("carol:pass"),
            header("alice"),
            "Bearer xyz".to_string(),
            "Basic !!!".to_string(),
        ] {
            assert!(!auth.verify(Some(&bad)).await, "{bad}");
        }
        assert!(!auth.verify(None).await);
    }

    #[tokio::test]
    async fn htpasswd_bcrypt_output_is_accepted() {
        // `htpasswd -nbB -C 4 user pass` ($2y$ prefix)
        let auth = BasicAuth::new(&[
            "user:$2y$04$N0Yr4/yCYpqgkg3HGBsHvegps1aikG8M.gXc5DbCdgtqxZa9B4eRS".into(),
        ])
        .unwrap();
        assert!(auth.verify(Some(&header("user:pass"))).await);
        assert!(!auth.verify(Some(&header("user:nope"))).await);
    }

    #[test]
    fn bad_entries_are_rejected() {
        for (entry, expected) in [
            ("nocolon", "must be"),
            (":pw", "empty user"),
            ("u:$2y$04$short", "invalid bcrypt"),
            ("u:$apr1$abc$def", "htpasswd -nB u"),
            ("u:{SHA}W6ph5Mm5Pz8GgiULbPgzG37mj9g=", "only bcrypt"),
        ] {
            let err = BasicAuth::new(&[entry.to_string()]).unwrap_err();
            assert!(err.contains(expected), "{entry}: {err}");
        }
        let err = BasicAuth::new(&["a:1".into(), "a:2".into()]).unwrap_err();
        assert!(err.contains("twice"), "{err}");
    }

    #[test]
    fn debug_output_hides_secrets() {
        let auth = BasicAuth::new(&["alice:hunter2".into()]).unwrap();
        let debug = format!("{auth:?}");
        assert!(
            debug.contains("alice") && !debug.contains("hunter2"),
            "{debug}"
        );
    }
}
