//! The subgraph/supergraph seam: each member's live `{ _service { sdl } }`
//! against the SDL its router serves.
//!
//! The application's build composes the supergraph from the committed SDL
//! files and the router serves it; fraisier never composes. What it checks is
//! that each deployed subgraph still serves the SDL the router's release was
//! composed from. A subgraph deployed with a new schema under a router still
//! serving the old supergraph is a silent break, so a difference fails.
//!
//! The comparison is the pilot's (`scripts/ci/live_sdl_matches.py`): exact,
//! except that trailing newlines are not a difference.

use std::fmt::Write as _;
use std::path::Path;

use async_trait::async_trait;
use fraisier_config::stack::Stack;

/// Where the two SDLs come from.
#[async_trait]
pub(crate) trait SdlSource: Send + Sync {
    /// The SDL the subgraph at `url` serves, asked with `headers`.
    ///
    /// # Errors
    /// The query could not be made or did not answer an SDL.
    async fn live_sdl(&self, url: &str, headers: &[(String, String)]) -> Result<String, String>;

    /// The SDL file at `path`, in the router's release.
    ///
    /// # Errors
    /// The file cannot be read.
    fn served_sdl(&self, path: &Path) -> Result<String, String>;
}

/// One member's check: `None` when its live SDL is the one the router serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SdlFinding {
    /// The member.
    pub(crate) member: String,
    /// Why it does not match, or could not be checked.
    pub(crate) problem: Option<String>,
    /// Whether both SDLs were read and compared; a problem without a
    /// comparison is a check that could not run, not a difference.
    pub(crate) compared: bool,
}

/// Where `live` first differs from `served`, or `None` when they agree.
pub(crate) fn sdl_difference(live: &str, served: &str) -> Option<String> {
    let (live, served) = (live.trim_end_matches('\n'), served.trim_end_matches('\n'));
    if live == served {
        return None;
    }
    let (mut live_lines, mut served_lines) = (live.lines(), served.lines());
    let mut number = 0;
    loop {
        number += 1;
        match (served_lines.next(), live_lines.next()) {
            (Some(a), Some(b)) if a == b => {}
            (a, b) => {
                let shown = |side: Option<&str>| {
                    side.map_or_else(|| "nothing".to_owned(), |text| format!("`{}`", clip(text)))
                };
                return Some(format!(
                    "they differ at line {number}: the router's release has {}, the subgraph \
                     serves {}",
                    shown(a),
                    shown(b)
                ));
            }
        }
    }
}

/// `text`, cut to a readable length.
fn clip(text: &str) -> String {
    const MAX: usize = 120;
    if text.chars().count() <= MAX {
        return text.to_owned();
    }
    format!("{}…", text.chars().take(MAX).collect::<String>())
}

/// `findings` as report lines, and whether every member matched.
pub(crate) fn render(findings: &[SdlFinding]) -> (String, bool) {
    let mut pretty = String::new();
    for finding in findings {
        match &finding.problem {
            None => {
                let _ = writeln!(pretty, "  sdl {}: matches", finding.member);
            }
            Some(why) => {
                let verdict = if finding.compared {
                    "DIFFERS"
                } else {
                    "UNCHECKED"
                };
                let _ = writeln!(pretty, "  sdl {}: {verdict}\n      {why}", finding.member);
            }
        }
    }
    (pretty, findings.iter().all(|f| f.problem.is_none()))
}

/// `findings` as JSON.
pub(crate) fn to_json(findings: &[SdlFinding]) -> serde_json::Value {
    findings
        .iter()
        .map(|finding| {
            serde_json::json!({
                "member": finding.member,
                "matches": finding.problem.is_none(),
                "checked": finding.compared,
                "problem": finding.problem,
            })
        })
        .collect()
}

/// Check every member with `sdl` against the router's release. Nothing is
/// checked in a stack without a router.
pub(crate) async fn check(
    stack: &Stack,
    source: &dyn SdlSource,
    env: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Vec<SdlFinding> {
    let Some(release) = stack
        .router
        .as_deref()
        .and_then(|router| stack.members.iter().find(|m| m.name == router))
        .and_then(|router| router.config.artifact.as_ref()?.active_path.clone())
    else {
        return Vec::new();
    };
    let mut findings = Vec::new();
    for member in &stack.members {
        let (Some(sdl), Some(url)) = (&member.sdl, &member.graphql_url) else {
            continue;
        };
        let (compared, problem) =
            match check_member(&release.join(sdl), url, &member.sdl_headers, source, env).await {
                Ok(difference) => (true, difference),
                Err(why) => (false, Some(why)),
            };
        findings.push(SdlFinding {
            member: member.name.clone(),
            problem: problem.map(|why| fraisier_saga::redact::credentials(&why)),
            compared,
        });
    }
    findings
}

/// Compare one member's SDLs: `Ok` with the difference, if any, once both were
/// read; `Err` when the check could not run.
async fn check_member(
    served_path: &Path,
    url: &str,
    header_vars: &std::collections::BTreeMap<String, String>,
    source: &dyn SdlSource,
    env: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Result<Option<String>, String> {
    let served = source
        .served_sdl(served_path)
        .map_err(|error| format!("cannot read {}: {error}", served_path.display()))?;
    let mut headers = Vec::with_capacity(header_vars.len());
    for (name, var) in header_vars {
        let value = env(var).filter(|v| !v.is_empty()).ok_or_else(|| {
            format!("cannot ask {url} for its SDL: ${var} is not set (header {name})")
        })?;
        headers.push((name.clone(), value));
    }
    let values: Vec<String> = headers.iter().map(|(_, v)| v.clone()).collect();
    let hide = |text: String| {
        values
            .iter()
            .fold(text, |text, value| text.replace(value.as_str(), "***"))
    };
    let live = source
        .live_sdl(url, &headers)
        .await
        .map_err(|error| hide(format!("cannot ask {url} for its SDL: {error}")))?;
    Ok(sdl_difference(&live, &served).map(|why| {
        hide(format!(
            "the live SDL at {url} is not {}: {why}. Recapture the SDL, recompose, and \
             deploy the router",
            served_path.display()
        ))
    }))
}

/// The real source: an HTTP POST of `{ _service { sdl } }`, and the file system.
pub(crate) struct LiveSdl;

#[async_trait]
impl SdlSource for LiveSdl {
    async fn live_sdl(&self, url: &str, headers: &[(String, String)]) -> Result<String, String> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| error.to_string())?;
        let mut request = client
            .post(url)
            .header("content-type", "application/json")
            .body(r#"{"query":"{ _service { sdl } }"}"#);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        let status = response.status();
        let body = response.text().await.map_err(|error| error.to_string())?;
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }
        parse_sdl_reply(&body)
    }

    fn served_sdl(&self, path: &Path) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|error| error.to_string())
    }
}

/// The SDL in a `{ _service { sdl } }` reply.
fn parse_sdl_reply(body: &str) -> Result<String, String> {
    let reply: serde_json::Value =
        serde_json::from_str(body).map_err(|error| format!("the reply is not JSON: {error}"))?;
    reply["data"]["_service"]["sdl"]
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("the _service query failed: {}", reply["errors"]))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use async_trait::async_trait;
    use fraisier_config::stack::{Stack, StackFile};

    use super::{check, sdl_difference, SdlFinding, SdlSource};
    use crate::test_env::block_on;

    const SDL: &str =
        "type Query {\n  orders: [Order!]!\n}\n\ntype Order @key(fields: \"id\") {\n  id: ID!\n}";

    #[test]
    fn trailing_newlines_are_not_a_difference() {
        assert_eq!(sdl_difference(&format!("{SDL}\n"), SDL), None);
        assert_eq!(sdl_difference(SDL, &format!("{SDL}\n\n")), None);
    }

    #[test]
    fn a_difference_names_its_first_line_and_both_sides() {
        let live = SDL.replace("orders: [Order!]!", "orders(first: Int): [Order!]!");
        let why = sdl_difference(&live, SDL).expect("differs");
        assert!(why.contains("line 2"), "{why}");
        assert!(why.contains("orders: [Order!]!"), "{why}");
        assert!(why.contains("orders(first: Int): [Order!]!"), "{why}");
        let why = sdl_difference(&format!("{SDL}\nscalar JSON"), SDL).expect("longer");
        assert!(why.contains("line 8"), "{why}");
        assert!(why.contains("scalar JSON"), "{why}");
    }

    #[test]
    fn indentation_is_a_difference() {
        assert!(sdl_difference(&SDL.replace("  id", "    id"), SDL).is_some());
    }

    /// Serves `live` per URL; reads `served` per path; records what it was asked.
    #[derive(Default)]
    struct Fake {
        live: BTreeMap<&'static str, Result<String, String>>,
        served: BTreeMap<PathBuf, String>,
        asked: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl SdlSource for Fake {
        async fn live_sdl(
            &self,
            url: &str,
            headers: &[(String, String)],
        ) -> Result<String, String> {
            let headers: Vec<String> = headers.iter().map(|(n, v)| format!("{n}={v}")).collect();
            self.asked
                .lock()
                .expect("asked")
                .push(format!("{url} [{}]", headers.join(",")));
            self.live
                .get(url)
                .cloned()
                .unwrap_or_else(|| Err("connection refused".to_owned()))
        }
        fn served_sdl(&self, path: &Path) -> Result<String, String> {
            self.served
                .get(path)
                .cloned()
                .ok_or_else(|| "No such file or directory".to_owned())
        }
    }

    fn stack(router: bool) -> Stack {
        let head = if router {
            "[stack]\nname = \"erp\"\nenvironment = \"staging\"\nrouter = \"gateway\"\n"
        } else {
            "[stack]\nname = \"erp\"\nenvironment = \"staging\"\n"
        };
        let members = if router {
            "[[stack.member]]\nconfig = \"gateway.toml\"\ndepends_on = [\"billing\", \"management\"]\n\
             [[stack.member]]\nconfig = \"management.toml\"\nsdl = \"federation/sdl/management.graphql\"\n\
             graphql_url = \"http://m/graphql\"\nsdl_headers = { \"x-api-key\" = \"M_TOKEN\" }\n\
             [[stack.member]]\nconfig = \"billing.toml\"\nsdl = \"federation/sdl/billing.graphql\"\n\
             graphql_url = \"http://b/graphql\"\n"
        } else {
            "[[stack.member]]\nconfig = \"management.toml\"\n"
        };
        let file = StackFile::from_toml_str(&format!("{head}{members}")).expect("parses");
        Stack::resolve(&file, Path::new("/s"), &|path| {
            let name = path
                .file_stem()
                .expect("stem")
                .to_string_lossy()
                .into_owned();
            let artifact = if name == "gateway" {
                "[artifact]\nsource = \"local\"\nactive_path = \"/srv/gateway/current\"\n"
            } else {
                ""
            };
            Ok(format!(
                "[deploy]\nname = \"{name}\"\nenvironment = \"staging\"\n{artifact}"
            ))
        })
        .expect("valid")
    }

    fn served(name: &str) -> PathBuf {
        Path::new("/srv/gateway/current/federation/sdl").join(format!("{name}.graphql"))
    }

    fn env(name: &str) -> Option<String> {
        (name == "M_TOKEN").then(|| "s3cret".to_owned())
    }

    #[test]
    fn each_member_with_sdl_is_checked_against_the_routers_release() {
        let other = SDL.replace("Order", "Invoice");
        let fake = Fake {
            live: BTreeMap::from([
                ("http://m/graphql", Ok(format!("{SDL}\n"))),
                ("http://b/graphql", Ok(other)),
            ]),
            served: BTreeMap::from([
                (served("management"), SDL.to_owned()),
                (served("billing"), SDL.to_owned()),
            ]),
            ..Fake::default()
        };
        let findings = block_on(check(&stack(true), &fake, &env));
        assert_eq!(
            findings[0],
            SdlFinding {
                member: "billing".to_owned(),
                problem: findings[0].problem.clone(),
                compared: true,
            }
        );
        let billing = findings[0].problem.as_deref().expect("billing differs");
        assert!(billing.contains("Invoice"), "{billing}");
        assert!(
            billing.contains("/srv/gateway/current/federation/sdl/billing.graphql"),
            "{billing}"
        );
        assert_eq!(
            findings[1],
            SdlFinding {
                member: "management".to_owned(),
                problem: None,
                compared: true,
            }
        );
        assert_eq!(findings.len(), 2, "the router has no sdl");
        assert_eq!(
            *fake.asked.lock().expect("asked"),
            vec!["http://b/graphql []", "http://m/graphql [x-api-key=s3cret]"]
        );
    }

    #[test]
    fn a_member_that_cannot_be_checked_fails_without_its_credential() {
        let fake = Fake {
            served: BTreeMap::from([(served("management"), SDL.to_owned())]),
            ..Fake::default()
        };
        let findings = block_on(check(&stack(true), &fake, &|_| None));
        assert!(!findings[0].compared && !findings[1].compared);
        let billing = findings[0].problem.as_deref().expect("unservable");
        assert!(billing.contains("No such file"), "{billing}");
        let management = findings[1].problem.as_deref().expect("no token");
        assert!(management.contains("$M_TOKEN is not set"), "{management}");
        assert!(
            fake.asked.lock().expect("asked").is_empty(),
            "nothing is asked without both sides"
        );

        let fake = Fake {
            served: BTreeMap::from([
                (served("management"), SDL.to_owned()),
                (served("billing"), SDL.to_owned()),
            ]),
            ..Fake::default()
        };
        let findings = block_on(check(&stack(true), &fake, &env));
        let management = findings[1].problem.as_deref().expect("refused");
        assert!(management.contains("connection refused"), "{management}");
        assert!(!management.contains("s3cret"), "{management}");
    }

    #[test]
    fn a_check_that_could_not_run_is_not_called_a_difference() {
        let finding = |problem: Option<&str>, compared| SdlFinding {
            member: "m".to_owned(),
            problem: problem.map(ToOwned::to_owned),
            compared,
        };
        let (pretty, ok) = super::render(&[
            finding(None, true),
            finding(Some("they differ at line 2"), true),
            finding(Some("cannot read sdl/m.graphql"), false),
        ]);
        assert!(!ok);
        assert_eq!(
            pretty,
            "  sdl m: matches\n  sdl m: DIFFERS\n      they differ at line 2\n  \
             sdl m: UNCHECKED\n      cannot read sdl/m.graphql\n"
        );
        let json = super::to_json(&[finding(Some("cannot read"), false)]);
        assert_eq!(json[0]["checked"], serde_json::json!(false));
        assert_eq!(json[0]["matches"], serde_json::json!(false));
    }

    #[test]
    fn a_stack_without_a_router_checks_nothing() {
        let fake = Fake::default();
        assert_eq!(
            block_on(check(&stack(false), &fake, &env)),
            Vec::<SdlFinding>::new()
        );
        assert!(fake.asked.lock().expect("asked").is_empty());
    }

    /// Serve one HTTP reply on a local port; the request it got comes back.
    fn serve_once(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/graphql", listener.local_addr().expect("addr"));
        let reply = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{body}",
            body.len()
        );
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            while !String::from_utf8_lossy(&request).contains("_service") {
                let n = stream.read(&mut buf).expect("read");
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
            }
            stream.write_all(reply.as_bytes()).expect("write");
            String::from_utf8_lossy(&request).into_owned()
        });
        (url, handle)
    }

    #[test]
    fn the_live_source_posts_the_service_query_with_its_headers() {
        let (url, server) = serve_once(
            "200 OK",
            r#"{"data":{"_service":{"sdl":"type Query { a: Int }"}}}"#,
        );
        let sdl = block_on(
            super::LiveSdl.live_sdl(&url, &[("x-api-key".to_owned(), "s3cret".to_owned())]),
        )
        .expect("an SDL");
        assert_eq!(sdl, "type Query { a: Int }");
        let request = server.join().expect("server").to_lowercase();
        assert!(request.starts_with("post /graphql"), "{request}");
        assert!(request.contains("x-api-key: s3cret"), "{request}");
        assert!(request.contains("{ _service { sdl } }"), "{request}");
    }

    #[test]
    fn the_live_source_reports_an_error_reply_or_status() {
        let (url, server) = serve_once("200 OK", r#"{"errors":[{"message":"forbidden"}]}"#);
        let why = block_on(super::LiveSdl.live_sdl(&url, &[])).expect_err("no data");
        server.join().expect("server");
        assert!(why.contains("forbidden"), "{why}");

        let (url, server) = serve_once("401 Unauthorized", "{}");
        let why = block_on(super::LiveSdl.live_sdl(&url, &[])).expect_err("401");
        server.join().expect("server");
        assert!(why.contains("401"), "{why}");
    }
}
