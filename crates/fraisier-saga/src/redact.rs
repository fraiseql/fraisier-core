//! Strip credentials out of operator-visible text.
//!
//! A saga reason is the most widely-copied string the engine produces: it is
//! printed, logged, exported to a collector, persisted to the state store and —
//! through a remote ledger — pushed off the host. Adapters build those reasons
//! out of whatever their tool wrote to stderr, and a database client that cannot
//! connect prints the DSN it tried, password included.
//!
//! So the rule is one-way: a reason may name *which* host could not be reached,
//! because that is the actionable half, and may never carry the credentials that
//! were used to reach it. This lives in the engine layer rather than in the
//! deploy layer because the engine is what persists and replicates a reason.

/// Strip credentials out of every URL and every libpq keyword/value pair in
/// `text`.
///
/// Two forms carry a password in practice:
///
/// - a URL's authority — `postgresql://user:pw@host/db` becomes
///   `postgresql://***@host/db`;
/// - libpq's keyword/value conninfo — `host=db password=pw` becomes
///   `host=db password=***`, which is what psycopg echoes when it reports a
///   conninfo it could not use.
///
/// Everything else is returned untouched, so a credential-free message is
/// byte-for-byte what the adapter wrote.
#[must_use]
pub fn credentials(text: &str) -> String {
    keywords(&urls(text))
}

/// Replace `user:password@` with `***` in every URL authority in `text`.
fn urls(text: &str) -> String {
    /// What ends a URL's authority section.
    const AUTHORITY_END: [char; 8] = ['/', '?', '#', ' ', '\t', '"', '\'', ','];
    /// What separates a scheme from the authority that may carry credentials.
    const MARK: &str = "://";

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(mark) = rest.find(MARK) {
        let (head, authority) = rest.split_at(mark + MARK.len());
        out.push_str(head);
        let end = authority.find(AUTHORITY_END).unwrap_or(authority.len());
        if let Some(at) = authority[..end].rfind('@') {
            out.push_str("***");
            rest = &authority[at..];
        } else {
            out.push_str(&authority[..end]);
            rest = &authority[end..];
        }
    }
    out.push_str(rest);
    out
}

/// Replace the value of every `password=` keyword in `text` with `***`.
///
/// libpq's keyword/value form separates pairs by whitespace and allows a value to
/// be single-quoted (`password='a b'`), which is the only way a value may contain
/// a space. A `password=` with no value is left alone — there is nothing to hide,
/// and rewriting it would invent a secret where the text said there was none.
fn keywords(text: &str) -> String {
    /// The keyword whose value is a secret.
    const KEY: &str = "password=";

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(KEY) {
        // Only a pair *starts* a keyword: `oldpassword=` is a different keyword,
        // and rewriting inside a longer word would corrupt unrelated prose.
        let boundary = rest[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        let (head, value) = rest.split_at(at + KEY.len());
        out.push_str(head);
        if !boundary {
            rest = value;
            continue;
        }
        // A quoted value ends at its closing quote; an unterminated quote runs to
        // the end of the text, which is still a secret. An unquoted one ends at
        // the whitespace that starts the next keyword.
        let end = value.strip_prefix('\'').map_or_else(
            || value.find(char::is_whitespace).unwrap_or(value.len()),
            |quoted| {
                quoted
                    .find('\'')
                    .map_or(value.len(), |close| close + "''".len())
            },
        );
        if end == 0 {
            rest = value;
            continue;
        }
        out.push_str("***");
        rest = &value[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::credentials;

    #[test]
    fn a_credential_free_url_is_returned_intact() {
        assert_eq!(
            credentials("connection to postgresql://db.internal:5432/checkout failed"),
            "connection to postgresql://db.internal:5432/checkout failed"
        );
    }

    #[test]
    fn every_url_authority_loses_its_userinfo() {
        assert_eq!(
            credentials("postgres://u:p@h/db and postgresql://a:b@c/d"),
            "postgres://***@h/db and postgresql://***@c/d"
        );
    }

    #[test]
    fn a_libpq_keyword_password_is_stripped_and_the_host_survives() {
        assert_eq!(
            credentials("connection failed: host=db.internal password=hunter2 dbname=checkout"),
            "connection failed: host=db.internal password=*** dbname=checkout"
        );
    }

    #[test]
    fn a_quoted_keyword_password_loses_the_whole_quoted_value() {
        assert_eq!(
            credentials("host=db password='hunter 2' dbname=checkout"),
            "host=db password=*** dbname=checkout"
        );
        // An unterminated quote runs to the end — still a secret, still stripped.
        assert_eq!(
            credentials("host=db password='hunter2"),
            "host=db password=***"
        );
    }

    #[test]
    fn a_trailing_keyword_password_is_stripped() {
        assert_eq!(
            credentials("host=db password=hunter2"),
            "host=db password=***"
        );
    }

    #[test]
    fn a_valueless_password_keyword_is_left_alone() {
        // There is no secret here; inventing `***` would claim there was one.
        assert_eq!(
            credentials("password= dbname=checkout"),
            "password= dbname=checkout"
        );
        assert_eq!(
            credentials("the password= keyword"),
            "the password= keyword"
        );
    }

    #[test]
    fn a_longer_keyword_ending_in_password_is_not_a_password() {
        // libpq has no `oldpassword`, but prose and other tools do, and rewriting
        // inside a longer word would corrupt text that holds no secret.
        assert_eq!(credentials("oldpassword=kept"), "oldpassword=kept");
    }

    #[test]
    fn both_forms_in_one_message_are_both_stripped() {
        assert_eq!(
            credentials("tried postgresql://u:pw@db/app then host=db password=pw2"),
            "tried postgresql://***@db/app then host=db password=***"
        );
    }
}
