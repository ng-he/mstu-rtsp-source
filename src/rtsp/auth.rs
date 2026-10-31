//! The two logins a camera asks for: basic, and digest over MD5.

use std::{
    format,
    time::{SystemTime, UNIX_EPOCH},
};

use mstu_media::base64;

use crate::md5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub user: String,
    pub password: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Challenge {
    Basic {
        realm: String,
    },
    Digest {
        realm: String,
        nonce: String,
        opaque: Option<String>,
        qop: Option<String>,
    },
}

impl Challenge {
    /// Reads a `WWW-Authenticate` header. None if it asks for something this
    /// plugin cannot answer, which is anything but these two.
    pub fn parse(header: &str) -> Option<Self> {
        let (scheme, rest) = match header.trim().split_once(' ') {
            Some((scheme, rest)) => (scheme, rest),
            None => (header.trim(), ""),
        };

        let fields = fields(rest);
        let field = |name: &str| {
            fields
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
        };

        if scheme.eq_ignore_ascii_case("basic") {
            return Some(Self::Basic {
                realm: field("realm").unwrap_or_default(),
            });
        }

        if scheme.eq_ignore_ascii_case("digest") {
            return Some(Self::Digest {
                realm: field("realm").unwrap_or_default(),
                nonce: field("nonce")?,
                opaque: field("opaque"),
                qop: field("qop"),
            });
        }

        None
    }

    /// The `Authorization` header for one request. `count` rises with every
    /// answer to the same challenge, as digest with a qop demands.
    pub fn answer(&self, credentials: &Credentials, method: &str, uri: &str, count: u32) -> String {
        match self {
            Self::Basic { .. } => {
                let login = format!("{}:{}", credentials.user, credentials.password);

                format!("Basic {}", base64::encode(login.as_bytes()))
            }

            Self::Digest {
                realm,
                nonce,
                opaque,
                qop,
            } => {
                let secret = md5::joined(&[&credentials.user, realm, &credentials.password]);
                let action = md5::joined(&[method, uri]);

                // A camera that asks for a qop wants the counter and a nonce
                // of ours mixed in; one that does not has never heard of them.
                let auth = qop
                    .as_deref()
                    .map(|qop| qop.split(',').any(|one| one.trim() == "auth"))
                    .unwrap_or(false);

                let nonce_count = format!("{count:08x}");
                let client_nonce = cnonce(count);

                let response = if auth {
                    md5::joined(&[&secret, nonce, &nonce_count, &client_nonce, "auth", &action])
                } else {
                    md5::joined(&[&secret, nonce, &action])
                };

                let mut header = format!(
                    "Digest username=\"{}\", realm=\"{realm}\", nonce=\"{nonce}\", \
                     uri=\"{uri}\", response=\"{response}\"",
                    credentials.user
                );

                if auth {
                    header.push_str(&format!(
                        ", qop=auth, nc={nonce_count}, cnonce=\"{client_nonce}\""
                    ));
                }

                if let Some(opaque) = opaque {
                    header.push_str(&format!(", opaque=\"{opaque}\""));
                }

                header
            }
        }
    }
}

/// A nonce of our own, which only has to differ from the last one.
fn cnonce(count: u32) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or(0);

    md5::hex(format!("{now}:{count}").as_bytes())[..16].to_string()
}

/// `realm="cam", nonce="xyz"` into its pairs, quotes off and commas inside
/// them left alone.
fn fields(text: &str) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    let mut name = String::new();
    let mut value = String::new();
    let mut quoted = false;
    let mut naming = true;

    let mut take = |name: &mut String, value: &mut String| {
        if !name.trim().is_empty() {
            fields.push((name.trim().to_string(), value.trim().to_string()));
        }

        name.clear();
        value.clear();
    };

    for character in text.chars() {
        match character {
            '"' => quoted = !quoted,

            '=' if naming && !quoted => naming = false,

            ',' if !quoted => {
                take(&mut name, &mut value);
                naming = true;
            }

            _ if naming => name.push(character),
            _ => value.push(character),
        }
    }

    take(&mut name, &mut value);
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login() -> Credentials {
        Credentials {
            user: "admin".to_string(),
            password: "1234".to_string(),
        }
    }

    #[test]
    fn a_basic_challenge_is_answered_with_the_login() {
        let challenge = Challenge::parse("Basic realm=\"cam\"").expect("a challenge");

        assert_eq!(
            challenge,
            Challenge::Basic {
                realm: "cam".to_string()
            }
        );

        let answer = challenge.answer(&login(), "DESCRIBE", "rtsp://cam/s", 1);

        assert_eq!(answer, "Basic YWRtaW46MTIzNA==");
        assert_eq!(
            base64::decode("YWRtaW46MTIzNA==").as_deref(),
            Some(b"admin:1234".as_slice())
        );
    }

    /// The worked example's shape: the response is the digest of the secret,
    /// the nonce and the action.
    #[test]
    fn a_digest_challenge_is_answered_with_the_three_digests() {
        let challenge =
            Challenge::parse("Digest realm=\"cam\", nonce=\"xyz\"").expect("a challenge");

        let answer = challenge.answer(&login(), "DESCRIBE", "rtsp://cam/s", 1);

        let secret = md5::joined(&["admin", "cam", "1234"]);
        let action = md5::joined(&["DESCRIBE", "rtsp://cam/s"]);
        let response = md5::joined(&[&secret, "xyz", &action]);

        assert!(
            answer.contains(&format!("response=\"{response}\"")),
            "{answer}"
        );
        assert!(answer.contains("username=\"admin\""));
        assert!(answer.contains("uri=\"rtsp://cam/s\""));
        assert!(!answer.contains("qop"), "none was asked for");
    }

    #[test]
    fn a_digest_with_a_qop_counts_and_adds_a_nonce_of_its_own() {
        let challenge = Challenge::parse("Digest realm=\"cam\", nonce=\"xyz\", qop=\"auth\"")
            .expect("a challenge");

        let answer = challenge.answer(&login(), "PLAY", "rtsp://cam/s", 7);

        assert!(answer.contains("qop=auth"), "{answer}");
        assert!(answer.contains("nc=00000007"), "{answer}");
        assert!(answer.contains("cnonce=\""), "{answer}");
    }

    /// The same request twice must not answer the same way, or a camera that
    /// checks the counter refuses the second.
    #[test]
    fn two_answers_to_one_challenge_differ() {
        let challenge = Challenge::parse("Digest realm=\"cam\", nonce=\"xyz\", qop=\"auth\"")
            .expect("a challenge");

        let first = challenge.answer(&login(), "PLAY", "rtsp://cam/s", 1);
        let second = challenge.answer(&login(), "PLAY", "rtsp://cam/s", 2);

        assert_ne!(first, second);
    }

    #[test]
    fn an_opaque_is_handed_straight_back() {
        let challenge = Challenge::parse("Digest realm=\"cam\", nonce=\"xyz\", opaque=\"keep-me\"")
            .expect("a challenge");

        assert!(
            challenge
                .answer(&login(), "SETUP", "rtsp://cam/s", 1)
                .contains("opaque=\"keep-me\"")
        );
    }

    #[test]
    fn a_challenge_this_cannot_answer_is_no_challenge() {
        assert!(Challenge::parse("Bearer realm=\"cam\"").is_none());
        assert!(
            Challenge::parse("Digest realm=\"cam\"").is_none(),
            "no nonce"
        );
        assert!(Challenge::parse("").is_none());
    }

    /// A realm with a comma in it is one field, not two.
    #[test]
    fn a_quoted_comma_stays_inside_its_field() {
        let challenge = Challenge::parse("Digest realm=\"cam, in the hall\", nonce=\"xyz\"")
            .expect("a challenge");

        assert_eq!(
            challenge,
            Challenge::Digest {
                realm: "cam, in the hall".to_string(),
                nonce: "xyz".to_string(),
                opaque: None,
                qop: None,
            }
        );
    }
}
