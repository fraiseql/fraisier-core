//! The subgraph/supergraph seam: each member's live `{ _service { sdl } }`
//! against the SDL its router serves.
//!
//! The application's build composes the supergraph from the committed SDL
//! files and the router serves it; fraisier never composes. What it checks is
//! that each subgraph serves the SDL the router's release was composed from. A
//! subgraph deployed with a new schema under a router still serving the old
//! supergraph is a silent break, so a difference fails.
//!
//! `stack deploy` prevents it: the router's candidate release is staged first,
//! and each subgraph's health step compares against it ([`SdlGate`]). A
//! difference fails that member's health, so its saga rolls it back and the
//! router is never committed. `stack status` detects it after the fact,
//! against the router's active release ([`check`]).
//!
//! The comparison is the pilot's (`scripts/ci/live_sdl_matches.py`): exact,
//! except that trailing newlines are not a difference.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fraisier_adapter_http::HttpHealth;
use fraisier_config::stack::{Member, Stack};
use fraisier_core::adapter_axes::{AdapterCtx, AdapterError, HealthAdapter, HealthStatus, HostId};

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

/// What one member's check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The live SDL is the one the router's release holds.
    Matches,
    /// Both SDLs were read, and they differ: why.
    Differs(String),
    /// The check could not run: why. A failure, but not a difference.
    Unchecked(String),
    /// The check was not run, on purpose: why (a dry run).
    NotChecked(String),
}

impl Verdict {
    /// Whether this verdict lets the command succeed.
    const fn ok(&self) -> bool {
        matches!(self, Self::Matches | Self::NotChecked(_))
    }

    /// Why the subgraph fails, if it does.
    fn problem(&self) -> Option<&str> {
        match self {
            Self::Differs(why) | Self::Unchecked(why) => Some(why),
            Self::Matches | Self::NotChecked(_) => None,
        }
    }

    /// `Ok`'s difference, if any, as a verdict; `Err` as one that did not run.
    /// Credentials are redacted from either.
    fn of(compared: Result<Option<String>, String>) -> Self {
        let redact = |why: String| fraisier_saga::redact::credentials(&why);
        match compared {
            Ok(None) => Self::Matches,
            Ok(Some(why)) => Self::Differs(redact(why)),
            Err(why) => Self::Unchecked(redact(why)),
        }
    }
}

/// One member's check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SdlFinding {
    /// The member.
    pub(crate) member: String,
    /// What the check found.
    pub(crate) verdict: Verdict,
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

/// `findings` as report lines, and whether every member passed.
pub(crate) fn render(findings: &[SdlFinding]) -> (String, bool) {
    let mut pretty = String::new();
    for finding in findings {
        let member = &finding.member;
        let _ = match &finding.verdict {
            Verdict::Matches => writeln!(pretty, "  sdl {member}: matches"),
            Verdict::Differs(why) => writeln!(pretty, "  sdl {member}: DIFFERS\n      {why}"),
            Verdict::Unchecked(why) => writeln!(pretty, "  sdl {member}: UNCHECKED\n      {why}"),
            Verdict::NotChecked(why) => writeln!(pretty, "  sdl {member}: not checked ({why})"),
        };
    }
    (pretty, findings.iter().all(|f| f.verdict.ok()))
}

/// `findings` as JSON. `matches` is `null` for a check not run on purpose.
pub(crate) fn to_json(findings: &[SdlFinding]) -> serde_json::Value {
    findings
        .iter()
        .map(|finding| {
            let (matches, checked) = match &finding.verdict {
                Verdict::Matches => (Some(true), true),
                Verdict::Differs(_) => (Some(false), true),
                Verdict::Unchecked(_) => (Some(false), false),
                Verdict::NotChecked(_) => (None, false),
            };
            let skipped = match &finding.verdict {
                Verdict::NotChecked(why) => Some(why),
                _ => None,
            };
            serde_json::json!({
                "member": finding.member,
                "matches": matches,
                "checked": checked,
                "problem": finding.verdict.problem(),
                "skipped": skipped,
            })
        })
        .collect()
}

/// Why a router deployed through `[hosts]` is not checked: its releases live
/// on its hosts, and fraisier reads SDL files only where it runs.
pub(crate) fn on_hosts(router: &str) -> String {
    format!(
        "the router '{router}' deploys through [hosts], so its release is on its hosts; \
         fraisier reads SDL files only on the host it runs on"
    )
}

/// Check every member with `sdl` against the router's *active* release, after
/// the fact (`stack status`). Nothing is checked in a stack without a router.
pub(crate) async fn check(stack: &Stack, source: &dyn SdlSource) -> Vec<SdlFinding> {
    let Some(router) = stack
        .router
        .as_deref()
        .and_then(|router| stack.members.iter().find(|m| m.name == router))
    else {
        return Vec::new();
    };
    let release = router
        .config
        .artifact
        .as_ref()
        .and_then(|a| a.active_path.clone());
    let mut findings = Vec::new();
    for member in &stack.members {
        let (Some(sdl), Some(url)) = (&member.sdl, member.graphql_url()) else {
            continue;
        };
        let verdict = match &release {
            _ if router.config.hosts.is_some() => Verdict::Unchecked(on_hosts(&router.name)),
            None => continue,
            Some(release) => {
                let ctx = crate::factory::health_ctx(&member.config);
                Verdict::of(
                    compare(&release.join(sdl), url, &ctx, &HttpHealth::new(), source).await,
                )
            }
        };
        findings.push(SdlFinding {
            member: member.name.clone(),
            verdict,
        });
    }
    findings
}

/// Compare one member's SDLs: `Ok` with the difference, if any, once both were
/// read; `Err` when the check could not run. The query sends the headers the
/// member's own health probe sends (`http`, resolving them from `ctx`); their
/// values never appear in the result.
async fn compare(
    served_path: &Path,
    url: &str,
    ctx: &AdapterCtx,
    http: &HttpHealth,
    source: &dyn SdlSource,
) -> Result<Option<String>, String> {
    let served = source
        .served_sdl(served_path)
        .map_err(|error| format!("cannot read {}: {error}", served_path.display()))?;
    let headers = http
        .resolve_headers(ctx)
        .await
        .map_err(|error| format!("cannot ask {url} for its SDL: {error}"))?;
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

/// One subgraph's check inside its deploy's health step, against the SDL file
/// in the router's *candidate* release. [`SdlGate::guard`] wraps the member's
/// health adapter: once the member is healthy, a difference, or a check that
/// cannot run, makes it unhealthy, so its saga rolls it back.
#[derive(Clone)]
pub(crate) struct SdlGate {
    member: String,
    served: PathBuf,
    url: String,
    source: Arc<dyn SdlSource>,
    /// The first failing verdict, or the last match; `None` until the member
    /// was healthy enough to be asked.
    verdict: Arc<Mutex<Option<Verdict>>>,
}

impl SdlGate {
    /// `member`'s gate: its live SDL at its `[federation].graphql_url` against
    /// `candidate`, the router's staged release. `None` without `sdl` or URL.
    pub(crate) fn new(
        member: &Member,
        candidate: &Path,
        source: Arc<dyn SdlSource>,
    ) -> Option<Self> {
        Some(Self {
            member: member.name.clone(),
            served: candidate.join(member.sdl.as_ref()?),
            url: member.graphql_url()?.to_owned(),
            source,
            verdict: Arc::new(Mutex::new(None)),
        })
    }

    /// `inner`, with this check after it.
    pub(crate) fn guard(&self, inner: Arc<dyn HealthAdapter>) -> Arc<dyn HealthAdapter> {
        Arc::new(Guarded {
            inner,
            gate: self.clone(),
            http: HttpHealth::new(),
        })
    }

    /// Keep `verdict`, unless an earlier host already failed.
    fn record(&self, verdict: Verdict) {
        if let Ok(mut kept) = self.verdict.lock() {
            if kept.as_ref().is_none_or(Verdict::ok) {
                *kept = Some(verdict);
            }
        }
    }

    /// What the check found, or `None` when the health step never asked.
    pub(crate) fn finding(&self) -> Option<SdlFinding> {
        let verdict = self.verdict.lock().ok()?.clone()?;
        Some(SdlFinding {
            member: self.member.clone(),
            verdict,
        })
    }
}

/// A member's health adapter, then its [`SdlGate`].
struct Guarded {
    inner: Arc<dyn HealthAdapter>,
    gate: SdlGate,
    /// Resolves the member's `[health]` headers for the query, its token
    /// provider at most once.
    http: HttpHealth,
}

#[async_trait]
impl HealthAdapter for Guarded {
    async fn check(&self, ctx: &AdapterCtx, host: &HostId) -> Result<HealthStatus, AdapterError> {
        let status = self.inner.check(ctx, host).await?;
        if !status.healthy {
            return Ok(status);
        }
        let gate = &self.gate;
        let verdict = Verdict::of(
            compare(
                &gate.served,
                &gate.url,
                ctx,
                &self.http,
                gate.source.as_ref(),
            )
            .await,
        );
        let problem = verdict.problem().map(ToOwned::to_owned);
        gate.record(verdict);
        Ok(problem.map_or(status, |why| HealthStatus {
            healthy: false,
            detail: Some(why),
        }))
    }
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

    use std::sync::Arc;

    use fraisier_core::adapter_axes::{
        AdapterCtx, AdapterError, HealthAdapter, HealthStatus, HostId,
    };

    use super::{check, sdl_difference, SdlFinding, SdlGate, SdlSource, Verdict};
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
        stack_with(router, "[\"printf\", \"tok\"]")
    }

    /// The stack, billing's bearer token printed by `token_command`.
    fn stack_with(router: bool, token_command: &str) -> Stack {
        let head = if router {
            "[stack]\nname = \"erp\"\nenvironment = \"staging\"\nrouter = \"gateway\"\n"
        } else {
            "[stack]\nname = \"erp\"\nenvironment = \"staging\"\n"
        };
        let members = if router {
            "[[stack.member]]\nconfig = \"gateway.toml\"\ndepends_on = [\"billing\", \"management\"]\n\
             [[stack.member]]\nconfig = \"management.toml\"\nsdl = \"federation/sdl/management.graphql\"\n\
             [[stack.member]]\nconfig = \"billing.toml\"\nsdl = \"federation/sdl/billing.graphql\"\n"
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
            let rest = match name.as_str() {
                "gateway" => {
                    "[artifact]\nsource = \"local\"\nactive_path = \"/srv/gateway/current\"\n"
                        .to_owned()
                }
                // The SDL query sends the member's own [health] headers...
                "management" => "[federation]\ngraphql_url = \"http://m/graphql\"\n\
                                 [health]\nadapter = \"http\"\nurl = \"http://m/health\"\n\
                                 headers = { \"x-api-key\" = \"s3cret\" }\n"
                    .to_owned(),
                // ...and the token its [health].token_provider resolves.
                _ => format!(
                    "[federation]\ngraphql_url = \"http://b/graphql\"\n\
                     [health]\nadapter = \"http\"\nurl = \"http://b/health\"\n\
                     [health.token_provider]\ntype = \"exec\"\ncommand = {token_command}\n"
                ),
            };
            Ok(format!(
                "[deploy]\nname = \"{name}\"\nenvironment = \"staging\"\n{rest}"
            ))
        })
        .expect("valid")
    }

    fn served(name: &str) -> PathBuf {
        Path::new("/srv/gateway/current/federation/sdl").join(format!("{name}.graphql"))
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
        let findings = block_on(check(&stack(true), &fake));
        assert_eq!(findings[0].member, "billing");
        let Verdict::Differs(billing) = &findings[0].verdict else {
            panic!("billing differs: {findings:?}");
        };
        assert!(billing.contains("Invoice"), "{billing}");
        assert!(
            billing.contains("/srv/gateway/current/federation/sdl/billing.graphql"),
            "{billing}"
        );
        assert_eq!(
            findings[1],
            SdlFinding {
                member: "management".to_owned(),
                verdict: Verdict::Matches,
            }
        );
        assert_eq!(findings.len(), 2, "the router has no sdl");
        assert_eq!(
            *fake.asked.lock().expect("asked"),
            vec![
                "http://b/graphql [Authorization=Bearer tok]",
                "http://m/graphql [x-api-key=s3cret]"
            ]
        );
    }

    #[test]
    fn a_member_that_cannot_be_checked_fails_without_its_credential() {
        let fake = Fake {
            served: BTreeMap::from([(served("billing"), SDL.to_owned())]),
            ..Fake::default()
        };
        let findings = block_on(check(&stack_with(true, "[\"false\"]"), &fake));
        let (Verdict::Unchecked(billing), Verdict::Unchecked(management)) =
            (&findings[0].verdict, &findings[1].verdict)
        else {
            panic!("neither could run: {findings:?}");
        };
        assert!(billing.contains("token"), "{billing}");
        assert!(management.contains("No such file"), "{management}");
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
        let findings = block_on(check(&stack(true), &fake));
        let Verdict::Unchecked(management) = &findings[1].verdict else {
            panic!("refused: {findings:?}");
        };
        assert!(management.contains("connection refused"), "{management}");
        assert!(!management.contains("s3cret"), "{management}");
    }

    fn finding(verdict: Verdict) -> SdlFinding {
        SdlFinding {
            member: "m".to_owned(),
            verdict,
        }
    }

    #[test]
    fn a_check_that_could_not_run_is_not_called_a_difference() {
        let (pretty, ok) = super::render(&[
            finding(Verdict::Matches),
            finding(Verdict::Differs("they differ at line 2".to_owned())),
            finding(Verdict::Unchecked("cannot read sdl/m.graphql".to_owned())),
        ]);
        assert!(!ok);
        assert_eq!(
            pretty,
            "  sdl m: matches\n  sdl m: DIFFERS\n      they differ at line 2\n  \
             sdl m: UNCHECKED\n      cannot read sdl/m.graphql\n"
        );
        let json = super::to_json(&[finding(Verdict::Unchecked("cannot read".to_owned()))]);
        assert_eq!(json[0]["checked"], serde_json::json!(false));
        assert_eq!(json[0]["matches"], serde_json::json!(false));
    }

    #[test]
    fn a_check_not_run_on_purpose_says_why_and_fails_nothing() {
        let (pretty, ok) = super::render(&[finding(Verdict::NotChecked("dry run".to_owned()))]);
        assert!(ok);
        assert_eq!(pretty, "  sdl m: not checked (dry run)\n");
        let json = super::to_json(&[finding(Verdict::NotChecked("dry run".to_owned()))]);
        assert_eq!(json[0]["matches"], serde_json::Value::Null);
        assert_eq!(json[0]["checked"], serde_json::json!(false));
        assert_eq!(json[0]["skipped"], serde_json::json!("dry run"));
    }

    #[test]
    fn a_router_on_hosts_is_unchecked_and_never_read_locally() {
        let mut stack = stack(true);
        let gateway = stack
            .members
            .iter_mut()
            .find(|m| m.name == "gateway")
            .expect("router");
        gateway.config.hosts = Some(fraisier_config::HostsSection::default());
        let fake = Fake {
            served: BTreeMap::from([
                (served("management"), SDL.to_owned()),
                (served("billing"), SDL.to_owned()),
            ]),
            ..Fake::default()
        };
        let findings = block_on(check(&stack, &fake));
        assert_eq!(findings.len(), 2);
        for finding in &findings {
            let Verdict::Unchecked(why) = &finding.verdict else {
                panic!("a [hosts] router is never green: {findings:?}");
            };
            assert!(
                why.contains("the router 'gateway' deploys through [hosts]"),
                "{why}"
            );
        }
        assert!(fake.asked.lock().expect("asked").is_empty());
    }

    /// A health adapter that answers `.0`.
    struct Up(bool);

    #[async_trait]
    impl HealthAdapter for Up {
        async fn check(&self, _: &AdapterCtx, _: &HostId) -> Result<HealthStatus, AdapterError> {
            Ok(HealthStatus {
                healthy: self.0,
                detail: Some("HTTP 200".to_owned()),
            })
        }
    }

    /// Billing's gate against a candidate release at `/srv/gateway/releases/v2`.
    fn gate(fake: Arc<Fake>) -> (SdlGate, PathBuf) {
        let stack = stack(true);
        let billing = stack
            .members
            .iter()
            .find(|m| m.name == "billing")
            .expect("billing");
        let candidate = Path::new("/srv/gateway/releases/v2");
        let gate = SdlGate::new(billing, candidate, fake).expect("billing has sdl");
        (gate, candidate.join("federation/sdl/billing.graphql"))
    }

    fn probe(health: &dyn HealthAdapter) -> HealthStatus {
        block_on(health.check(
            &AdapterCtx::new("billing", "staging"),
            &HostId::new("localhost"),
        ))
        .expect("probed")
    }

    #[test]
    fn the_gate_fails_a_healthy_subgraph_its_candidate_router_release_does_not_hold() {
        let candidate = PathBuf::from("/srv/gateway/releases/v2/federation/sdl/billing.graphql");
        let fake = Arc::new(Fake {
            live: BTreeMap::from([("http://b/graphql", Ok(SDL.replace("Order", "Invoice")))]),
            served: BTreeMap::from([(candidate.clone(), SDL.to_owned())]),
            ..Fake::default()
        });
        let (gate, served) = gate(Arc::clone(&fake));
        assert_eq!(served, candidate);
        let status = probe(gate.guard(Arc::new(Up(true))).as_ref());
        assert!(!status.healthy, "{status:?}");
        let detail = status.detail.expect("why");
        assert!(
            detail.contains(&candidate.display().to_string()),
            "{detail}"
        );
        assert!(detail.contains("Invoice"), "{detail}");
        assert!(matches!(
            gate.finding().expect("asked").verdict,
            Verdict::Differs(_)
        ));
    }

    #[test]
    fn the_gate_passes_a_matching_subgraph_and_never_asks_an_unhealthy_one() {
        let candidate = PathBuf::from("/srv/gateway/releases/v2/federation/sdl/billing.graphql");
        let fake = Arc::new(Fake {
            live: BTreeMap::from([("http://b/graphql", Ok(SDL.to_owned()))]),
            served: BTreeMap::from([(candidate, SDL.to_owned())]),
            ..Fake::default()
        });
        let (gate, _) = gate(Arc::clone(&fake));
        assert!(!probe(gate.guard(Arc::new(Up(false))).as_ref()).healthy);
        assert_eq!(gate.finding(), None, "an unhealthy subgraph is not asked");
        assert!(fake.asked.lock().expect("asked").is_empty());

        let status = probe(gate.guard(Arc::new(Up(true))).as_ref());
        assert!(status.healthy, "{status:?}");
        assert_eq!(gate.finding().expect("asked").verdict, Verdict::Matches);
    }

    #[test]
    fn the_gate_fails_closed_when_its_check_cannot_run() {
        let fake = Arc::new(Fake::default());
        let (gate, served) = gate(fake);
        let status = probe(gate.guard(Arc::new(Up(true))).as_ref());
        assert!(!status.healthy, "{status:?}");
        assert!(
            status
                .detail
                .as_deref()
                .is_some_and(|d| d.contains(&format!("cannot read {}", served.display()))),
            "{status:?}"
        );
        assert!(matches!(
            gate.finding().expect("asked").verdict,
            Verdict::Unchecked(_)
        ));
    }

    #[test]
    fn a_stack_without_a_router_checks_nothing() {
        let fake = Fake::default();
        assert_eq!(
            block_on(check(&stack(false), &fake)),
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
