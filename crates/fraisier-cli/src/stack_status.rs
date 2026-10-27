//! `fraisier stack status`: each member's health, deployed version and last
//! deploy, and a row for each database the members declare
//! `[database.requires]` for.
//!
//! A member's health is its own `[health]` check, the one `fraisier health`
//! runs. A database is probed once per distinct DSN, against every member's
//! requirements. With a router, each subgraph's live SDL is checked against
//! the router's release (see [`crate::stack_sdl`]). The command exits non-zero
//! when a member is unhealthy, a database is not ready or an SDL differs; the
//! last deploy is reported, never judged.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use fraisier_config::stack::{Member, Stack};
use fraisier_saga::state_store::{FilesystemStateStore, FraiseKey, StateStore};
use serde_json::{json, Value};

use crate::commands::CommandOutput;
use crate::stack_sdl::SdlSource;

/// What `stack status` asks of the live system.
#[async_trait]
pub(crate) trait StackProbe: Send + Sync {
    /// `Ok` when every host of `member` answers healthy, otherwise why not.
    async fn health(&self, member: &Member) -> Result<(), String>;

    /// The member's last recorded deploy, or `None` when it never deployed.
    async fn last_deploy(&self, member: &Member) -> Option<LastDeploy>;

    /// Every reason the database at `dsn` falls short of `member`'s
    /// `[database.requires]`. Empty means ready.
    ///
    /// # Errors
    /// The probe could not run.
    async fn database_unready(&self, member: &Member, dsn: &str) -> Result<Vec<String>, String>;
}

/// A member's last recorded deploy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LastDeploy {
    /// The saga's last recorded state (`committed`, `rolled_back`, …).
    pub(crate) outcome: String,
    /// When it was recorded (RFC 3339).
    pub(crate) at: String,
    /// The live version, from the committed ledger.
    pub(crate) version: Option<String>,
}

/// One member's row.
struct MemberRow {
    name: String,
    unhealthy: Option<String>,
    last: Option<LastDeploy>,
}

/// One database's row: every member that declares it, and every problem found.
struct DatabaseRow {
    key: String,
    label: String,
    members: Vec<String>,
    problems: Vec<String>,
}

impl DatabaseRow {
    fn add_problem(&mut self, problem: &str) {
        let problem = fraisier_saga::redact::credentials(problem);
        if !self.problems.contains(&problem) {
            self.problems.push(problem);
        }
    }
}

/// The database `member` declares: the key rows are merged on, the label
/// shown (never a password), and its DSN or why there is none.
fn database_of(
    member: &Member,
    env: &(dyn Fn(&str) -> Option<String> + Sync),
) -> (String, String, Result<String, String>) {
    let Some(var) = member
        .config
        .migration
        .as_ref()
        .and_then(|m| m.database_url_env.as_deref())
    else {
        return (
            format!("{}:", member.name),
            format!("of {}", member.name),
            Err("[migration].database_url_env is not set".to_owned()),
        );
    };
    let Some(dsn) = env(var).filter(|dsn| !dsn.is_empty()) else {
        return (
            format!("${var}"),
            format!("${var}"),
            Err(format!("${var} is not set")),
        );
    };
    (
        dsn.clone(),
        fraisier_saga::redact::credentials(&dsn),
        Ok(dsn),
    )
}

/// Probe each database the members declare `[database.requires]` for, once
/// per distinct DSN, against every declaring member's requirements.
async fn databases(
    stack: &Stack,
    probe: &dyn StackProbe,
    env: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Vec<DatabaseRow> {
    let mut rows: Vec<DatabaseRow> = Vec::new();
    for member in &stack.members {
        if crate::pg_readiness::requirements(&member.config).is_none() {
            continue;
        }
        let (key, label, dsn) = database_of(member, env);
        let index = rows
            .iter()
            .position(|row| row.key == key)
            .unwrap_or_else(|| {
                rows.push(DatabaseRow {
                    key,
                    label,
                    members: Vec::new(),
                    problems: Vec::new(),
                });
                rows.len() - 1
            });
        let row = &mut rows[index];
        row.members.push(member.name.clone());
        match dsn {
            Err(problem) => row.add_problem(&problem),
            Ok(dsn) => match probe.database_unready(member, &dsn).await {
                Ok(problems) => problems.iter().for_each(|p| row.add_problem(p)),
                Err(error) => row.add_problem(&format!("cannot probe it: {error}")),
            },
        }
    }
    rows
}

fn render(
    stack: &Stack,
    members: &[MemberRow],
    databases: &[DatabaseRow],
    healthy: bool,
) -> String {
    let verdict = if healthy { "healthy" } else { "UNHEALTHY" };
    let mut pretty = format!("stack {} ({}): {verdict}\n", stack.name, stack.environment);
    let version = |row: &MemberRow| {
        row.last
            .as_ref()
            .and_then(|last| last.version.clone())
            .unwrap_or_else(|| "-".to_owned())
    };
    let name_width = members.iter().map(|row| row.name.len()).max().unwrap_or(0);
    let version_width = members
        .iter()
        .map(|row| version(row).len())
        .max()
        .unwrap_or(0);
    for row in members {
        let health = if row.unhealthy.is_some() {
            "UNHEALTHY"
        } else {
            "ok"
        };
        let last = row.last.as_ref().map_or_else(
            || "never deployed".to_owned(),
            |last| format!("{} {}", last.outcome, last.at),
        );
        let _ = writeln!(
            pretty,
            "  {:<name_width$}  {health:<9}  {:<version_width$}  {last}",
            row.name,
            version(row)
        );
        if let Some(why) = &row.unhealthy {
            let _ = writeln!(pretty, "      {why}");
        }
    }
    for row in databases {
        let state = if row.problems.is_empty() {
            "ready"
        } else {
            "NOT READY"
        };
        let _ = writeln!(
            pretty,
            "  database {} ({}): {state}",
            row.label,
            row.members.join(", ")
        );
        for problem in &row.problems {
            let _ = writeln!(pretty, "      {problem}");
        }
    }
    pretty
}

fn to_json(
    stack: &Stack,
    members: &[MemberRow],
    databases: &[DatabaseRow],
    healthy: bool,
) -> Value {
    json!({
        "stack": stack.name,
        "environment": stack.environment,
        "healthy": healthy,
        "members": members.iter().map(|row| json!({
            "name": row.name,
            "healthy": row.unhealthy.is_none(),
            "detail": row.unhealthy,
            "version": row.last.as_ref().and_then(|last| last.version.clone()),
            "last_deploy": row.last.as_ref().map(|last| json!({
                "outcome": last.outcome,
                "at": last.at,
            })),
        })).collect::<Vec<_>>(),
        "databases": databases.iter().map(|row| json!({
            "database": row.label,
            "members": row.members,
            "ready": row.problems.is_empty(),
            "problems": row.problems,
        })).collect::<Vec<_>>(),
    })
}

/// `stack status` with the probe and environment supplied by the caller.
pub(crate) async fn stack_status_with(
    stack_path: &Path,
    probe: &dyn StackProbe,
    sdl: &dyn SdlSource,
    env: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Result<CommandOutput> {
    let stack = match crate::stack::load(stack_path) {
        Ok(stack) => stack,
        Err(error) => {
            let message = error.to_string();
            return Ok(CommandOutput {
                exit_code: 1,
                pretty: format!("{message}\n"),
                json: json!({ "ok": false, "error": message }),
            });
        }
    };
    let mut members = Vec::with_capacity(stack.members.len());
    for member in &stack.members {
        members.push(MemberRow {
            name: member.name.clone(),
            unhealthy: probe
                .health(member)
                .await
                .err()
                .map(|why| fraisier_saga::redact::credentials(&why)),
            last: probe.last_deploy(member).await,
        });
    }
    let databases = databases(&stack, probe, env).await;
    let findings = crate::stack_sdl::check(&stack, sdl).await;
    let (sdl_pretty, sdl_ok) = crate::stack_sdl::render(&findings);
    let healthy = sdl_ok
        && members.iter().all(|row| row.unhealthy.is_none())
        && databases.iter().all(|row| row.problems.is_empty());
    let mut json = to_json(&stack, &members, &databases, healthy);
    json["sdl"] = crate::stack_sdl::to_json(&findings);
    Ok(CommandOutput {
        exit_code: i32::from(!healthy),
        pretty: render(&stack, &members, &databases, healthy) + &sdl_pretty,
        json,
    })
}

/// The real probe: `fraisier health` per member, the state store, and the
/// database readiness probe.
pub(crate) struct LiveProbe {
    /// The state store directory every member shares.
    pub(crate) state_dir: PathBuf,
}

/// `stack status` against the live system.
pub(crate) async fn stack_status(stack_path: &Path, probe: &LiveProbe) -> Result<CommandOutput> {
    stack_status_with(stack_path, probe, &crate::stack_sdl::LiveSdl, &|name| {
        std::env::var(name).ok()
    })
    .await
}

#[async_trait]
impl StackProbe for LiveProbe {
    async fn health(&self, member: &Member) -> Result<(), String> {
        let output = crate::commands::health(&member.config_path, None, &self.state_dir)
            .await
            .map_err(|error| crate::commands::error_detail(&error))?;
        if output.exit_code == 0 {
            return Ok(());
        }
        let hosts = output.json["hosts"].as_array().cloned().unwrap_or_default();
        let failing: Vec<String> = hosts
            .iter()
            .filter(|host| host["healthy"] != json!(true))
            .map(|host| {
                let name = host["host"].as_str().unwrap_or("?");
                host["detail"]
                    .as_str()
                    .map_or_else(|| name.to_owned(), |detail| format!("{name}: {detail}"))
            })
            .collect();
        Err(if failing.is_empty() {
            output.pretty.trim().to_owned()
        } else {
            failing.join("; ")
        })
    }

    async fn last_deploy(&self, member: &Member) -> Option<LastDeploy> {
        let store = FilesystemStateStore::new(&self.state_dir).ok()?;
        let key = FraiseKey::new(
            &member.name,
            member.config.deploy.as_ref()?.environment.as_deref()?,
        );
        let state = store.current_state(&key).await.ok()??;
        let (version, _) =
            crate::commands::committed_version(&member.config, &self.state_dir).await;
        Some(LastDeploy {
            outcome: state.state.to_string(),
            at: state.recorded_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            version,
        })
    }

    async fn database_unready(&self, member: &Member, dsn: &str) -> Result<Vec<String>, String> {
        match crate::pg_readiness::probe(&member.config) {
            Some(probe) => probe.unready(dsn).await,
            None => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use async_trait::async_trait;
    use fraisier_config::stack::Member;

    use super::{stack_status_with, LastDeploy, StackProbe};
    use crate::commands::CommandOutput;
    use crate::test_env::block_on;

    /// Answers from tables keyed by member name; records each database probe.
    #[derive(Default)]
    struct Fake {
        unhealthy: BTreeMap<&'static str, &'static str>,
        deploys: BTreeMap<&'static str, LastDeploy>,
        unready: BTreeMap<&'static str, Vec<String>>,
        probed: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl StackProbe for Fake {
        async fn health(&self, member: &Member) -> Result<(), String> {
            self.unhealthy
                .get(member.name.as_str())
                .map_or(Ok(()), |why| Err((*why).to_owned()))
        }
        async fn last_deploy(&self, member: &Member) -> Option<LastDeploy> {
            self.deploys.get(member.name.as_str()).cloned()
        }
        async fn database_unready(
            &self,
            member: &Member,
            dsn: &str,
        ) -> Result<Vec<String>, String> {
            self.probed
                .lock()
                .expect("probed")
                .push(format!("{}@{dsn}", member.name));
            if dsn.contains("down") {
                return Err(format!("could not connect to {dsn}"));
            }
            Ok(self
                .unready
                .get(member.name.as_str())
                .cloned()
                .unwrap_or_default())
        }
    }

    /// A stack without a router never asks for an SDL.
    struct NoSdl;

    #[async_trait]
    impl crate::stack_sdl::SdlSource for NoSdl {
        async fn live_sdl(&self, url: &str, _: &[(String, String)]) -> Result<String, String> {
            panic!("asked {url} for its SDL")
        }
        fn served_sdl(&self, path: &Path) -> Result<String, String> {
            panic!("read {}", path.display())
        }
    }

    /// Every SDL matches but billing's.
    struct BillingDiffers;

    #[async_trait]
    impl crate::stack_sdl::SdlSource for BillingDiffers {
        async fn live_sdl(&self, url: &str, _: &[(String, String)]) -> Result<String, String> {
            Ok(if url.contains("billing") {
                "type Query { invoices: Int }".to_owned()
            } else {
                "type Query { orders: Int }".to_owned()
            })
        }
        fn served_sdl(&self, _: &Path) -> Result<String, String> {
            Ok("type Query { orders: Int }".to_owned())
        }
    }

    #[test]
    fn an_sdl_the_router_does_not_serve_fails_the_status_naming_the_member() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = stack_dir(dir.path());
        std::fs::write(
            dir.path().join("router.toml"),
            "[deploy]\nname = \"router\"\nenvironment = \"staging\"\n\
             [artifact]\nsource = \"local\"\nactive_path = \"/srv/router/current\"\n",
        )
        .expect("write");
        std::fs::write(
            &path,
            STACK
                .replace(
                    "environment = \"staging\"\n",
                    "environment = \"staging\"\nrouter = \"router\"\n",
                )
                .replace(
                    "config = \"billing.toml\"\n",
                    "config = \"billing.toml\"\nsdl = \"sdl/billing.graphql\"\n",
                )
                .replace(
                    "config = \"management.toml\"\n",
                    "config = \"management.toml\"\nsdl = \"sdl/management.graphql\"\n",
                ),
        )
        .expect("write");
        for name in ["billing", "management"] {
            let config = dir.path().join(format!("{name}.toml"));
            let toml = std::fs::read_to_string(&config).expect("read");
            std::fs::write(
                &config,
                format!("{toml}[federation]\ngraphql_url = \"http://{name}/graphql\"\n"),
            )
            .expect("write");
        }
        let out = block_on(stack_status_with(
            &path,
            &Fake::default(),
            &BillingDiffers,
            &|_| Some("postgres://app@db/erp".to_owned()),
        ))
        .expect("runs");
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(
            out.pretty
                .contains("  sdl billing: DIFFERS\n      the live SDL at http://billing/graphql"),
            "{}",
            out.pretty
        );
        assert!(
            out.pretty.ends_with("  sdl management: matches\n"),
            "{}",
            out.pretty
        );
        assert_eq!(out.json["sdl"][0]["member"], serde_json::json!("billing"));
        assert_eq!(out.json["sdl"][0]["matches"], serde_json::json!(false));
        assert_eq!(out.json["sdl"][1]["matches"], serde_json::json!(true));
    }

    const STACK: &str = "[stack]\nname = \"erp\"\nenvironment = \"staging\"\n\n\
                         [[stack.member]]\nconfig = \"router.toml\"\n\
                         depends_on = [\"billing\", \"management\"]\n\n\
                         [[stack.member]]\nconfig = \"management.toml\"\n\n\
                         [[stack.member]]\nconfig = \"billing.toml\"\n";

    const REQUIRES: &str = "[database.requires]\npostgres_major = 18\n";

    /// A stack whose subgraphs share one database (`$ERP_DSN`); the router
    /// declares none.
    fn stack_dir(dir: &Path) -> PathBuf {
        for (name, extra) in [
            ("router", String::new()),
            (
                "management",
                format!("[migration]\ndatabase_url_env = \"ERP_DSN\"\n{REQUIRES}"),
            ),
            (
                "billing",
                format!("[migration]\ndatabase_url_env = \"ERP_DSN\"\n{REQUIRES}"),
            ),
        ] {
            std::fs::write(
                dir.join(format!("{name}.toml")),
                format!("[deploy]\nname = \"{name}\"\nenvironment = \"staging\"\n{extra}"),
            )
            .expect("write");
        }
        let path = dir.join("stack.staging.toml");
        std::fs::write(&path, STACK).expect("write");
        path
    }

    fn deployed(version: &str) -> LastDeploy {
        LastDeploy {
            outcome: "committed".to_owned(),
            at: "2026-10-26T10:00:00Z".to_owned(),
            version: Some(version.to_owned()),
        }
    }

    fn status(dir: &Path, fake: &Fake, dsn: &str) -> CommandOutput {
        let path = stack_dir(dir);
        let dsn = dsn.to_owned();
        block_on(stack_status_with(&path, fake, &NoSdl, &move |name| {
            (name == "ERP_DSN").then(|| dsn.clone())
        }))
        .expect("runs")
    }

    #[test]
    fn a_healthy_stack_renders_its_members_and_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = Fake {
            deploys: BTreeMap::from([
                ("billing", deployed("v1.4.0")),
                ("management", deployed("v2.0.1")),
            ]),
            ..Fake::default()
        };
        let out = status(dir.path(), &fake, "postgres://app:s3cret@db/erp");
        assert_eq!(out.exit_code, 0, "{}", out.pretty);
        assert_eq!(
            out.pretty,
            "stack erp (staging): healthy\n\
             \x20 billing     ok         v1.4.0  committed 2026-10-26T10:00:00Z\n\
             \x20 management  ok         v2.0.1  committed 2026-10-26T10:00:00Z\n\
             \x20 router      ok         -       never deployed\n\
             \x20 database postgres://***@db/erp (billing, management): ready\n"
        );
        assert_eq!(
            *fake.probed.lock().expect("probed"),
            vec![
                "billing@postgres://app:s3cret@db/erp",
                "management@postgres://app:s3cret@db/erp"
            ],
            "one row, every member's requirements"
        );
    }

    #[test]
    fn the_json_carries_every_member_and_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = Fake {
            deploys: BTreeMap::from([("billing", deployed("v1.4.0"))]),
            ..Fake::default()
        };
        let out = status(dir.path(), &fake, "postgres://app:s3cret@db/erp");
        assert_eq!(
            out.json,
            serde_json::json!({
                "stack": "erp",
                "environment": "staging",
                "healthy": true,
                "members": [
                    {
                        "name": "billing",
                        "healthy": true,
                        "detail": null,
                        "version": "v1.4.0",
                        "last_deploy": { "outcome": "committed", "at": "2026-10-26T10:00:00Z" },
                    },
                    {
                        "name": "management",
                        "healthy": true,
                        "detail": null,
                        "version": null,
                        "last_deploy": null,
                    },
                    {
                        "name": "router",
                        "healthy": true,
                        "detail": null,
                        "version": null,
                        "last_deploy": null,
                    },
                ],
                "databases": [
                    {
                        "database": "postgres://***@db/erp",
                        "members": ["billing", "management"],
                        "ready": true,
                        "problems": [],
                    },
                ],
                "sdl": [],
            })
        );
    }

    #[test]
    fn an_unhealthy_member_fails_the_status_and_says_why() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = Fake {
            unhealthy: BTreeMap::from([("management", "localhost: HTTP 503 from /health")]),
            ..Fake::default()
        };
        let out = status(dir.path(), &fake, "postgres://app:s3cret@db/erp");
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(
            out.pretty.starts_with("stack erp (staging): UNHEALTHY\n"),
            "{}",
            out.pretty
        );
        assert!(
            out.pretty.contains(
                "  management  UNHEALTHY  -  never deployed\n      localhost: HTTP 503 from /health\n"
            ),
            "{}",
            out.pretty
        );
        assert_eq!(out.json["healthy"], serde_json::json!(false));
        assert_eq!(
            out.json["members"][1]["detail"],
            serde_json::json!("localhost: HTTP 503 from /health")
        );
    }

    #[test]
    fn an_unready_database_fails_the_status_and_names_each_problem_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let problem = "PostgreSQL 16 is older than the required 18".to_owned();
        let fake = Fake {
            unready: BTreeMap::from([
                ("billing", vec![problem.clone()]),
                ("management", vec![problem.clone()]),
            ]),
            ..Fake::default()
        };
        let out = status(dir.path(), &fake, "postgres://app:s3cret@db/erp");
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(
            out.pretty.ends_with(
                "  database postgres://***@db/erp (billing, management): NOT READY\n      \
                 PostgreSQL 16 is older than the required 18\n"
            ),
            "{}",
            out.pretty
        );
        assert_eq!(
            out.json["databases"][0]["problems"],
            serde_json::json!([problem])
        );
    }

    #[test]
    fn an_unreachable_or_unnamed_database_is_not_ready_and_never_shows_its_password() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = status(
            dir.path(),
            &Fake::default(),
            "postgres://app:s3cret@down/erp",
        );
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(out.pretty.contains("could not connect"), "{}", out.pretty);
        assert!(!out.pretty.contains("s3cret"), "{}", out.pretty);
        assert!(!out.json.to_string().contains("s3cret"));

        let path = stack_dir(dir.path());
        let fake = Fake::default();
        let out = block_on(stack_status_with(&path, &fake, &NoSdl, &|_| None)).expect("runs");
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(
            out.pretty.contains(
                "  database $ERP_DSN (billing, management): NOT READY\n      $ERP_DSN is not set\n"
            ),
            "{}",
            out.pretty
        );
        assert!(fake.probed.lock().expect("probed").is_empty());
    }

    #[test]
    fn an_invalid_stack_probes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = stack_dir(dir.path());
        std::fs::remove_file(dir.path().join("billing.toml")).expect("rm");
        let fake = Fake::default();
        let out = block_on(stack_status_with(&path, &fake, &NoSdl, &|_| None)).expect("runs");
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(out.pretty.contains("billing.toml"), "{}", out.pretty);
        assert!(fake.probed.lock().expect("probed").is_empty());
    }

    fn live_member(dir: &Path) -> Member {
        let path = stack_dir(dir);
        let stack = fraisier_config::stack::Stack::load(&path).expect("valid");
        stack
            .members
            .into_iter()
            .find(|m| m.name == "billing")
            .expect("billing")
    }

    #[test]
    fn the_live_probe_reads_the_last_deploy_from_the_state_store() {
        use fraisier_saga::events::SagaState;
        use fraisier_saga::state_store::{
            DeploymentState, FilesystemStateStore, FraiseKey, StateStore,
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let member = live_member(dir.path());
        let probe = super::LiveProbe {
            state_dir: dir.path().join("state"),
        };
        assert_eq!(block_on(probe.last_deploy(&member)), None, "never deployed");

        let store = FilesystemStateStore::new(&probe.state_dir).expect("store");
        block_on(store.record_state(
            &FraiseKey::new("billing", "staging"),
            &DeploymentState::new(SagaState::RolledBack, None),
        ))
        .expect("record");
        let last = block_on(probe.last_deploy(&member)).expect("recorded");
        assert_eq!(last.outcome, "rolled_back");
        assert_eq!(last.version, None, "a rolled-back deploy committed nothing");
        assert!(
            last.at.len() == 20 && last.at.ends_with('Z'),
            "RFC 3339 to the second: {}",
            last.at
        );
    }

    #[test]
    fn the_live_probe_reports_a_member_it_cannot_check_as_unhealthy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let member = live_member(dir.path());
        let probe = super::LiveProbe {
            state_dir: dir.path().join("state"),
        };
        let why = block_on(probe.health(&member)).expect_err("no [health]");
        assert!(why.contains("[health]"), "{why}");
    }
}
