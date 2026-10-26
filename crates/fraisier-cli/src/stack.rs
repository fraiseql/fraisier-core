//! `fraisier stack deploy`: a stack's members, deployed one after another in
//! dependency order.
//!
//! Each member deploys through the same path `fraisier deploy` takes, with its
//! own config, saga and rollback. There is no rollback across members: a member
//! that fails rolls itself back, the members after it never start, and the
//! members before it stay deployed. The report says which is which.
//!
//! With a router, its candidate release is staged before any member deploys,
//! and each subgraph's health step checks its live SDL against that release
//! (see [`crate::stack_sdl`]). A difference fails the subgraph, which rolls
//! itself back, so the stack stops before the router is committed.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use fraisier_config::stack::{Member, Stack};
use serde_json::json;

use crate::commands::CommandOutput;
use crate::stack_sdl::{SdlGate, SdlSource};

/// Deploys one member.
#[async_trait]
pub(crate) trait MemberDeployer: Send + Sync {
    /// Deploy `member` at `app_version`, as `fraisier deploy` would, with
    /// `gate` after its health check.
    async fn deploy(
        &self,
        member: &Member,
        app_version: Option<&str>,
        gate: Option<&SdlGate>,
    ) -> Result<CommandOutput>;

    /// Stage `router`'s release at `app_version` without activating it, and
    /// say where it is.
    async fn stage(&self, router: &Member, app_version: Option<&str>) -> Result<PathBuf>;
}

/// What happened to one member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemberOutcome {
    /// Its deploy committed.
    Deployed,
    /// Its deploy failed (and rolled itself back), with why.
    Failed(String),
    /// It never started, because an earlier member failed.
    Skipped,
}

/// Deploy `stack`'s members in order, stopping at the first that fails.
pub(crate) async fn deploy_members(
    stack: &Stack,
    versions: &BTreeMap<String, String>,
    deployer: &dyn MemberDeployer,
    gates: &BTreeMap<String, SdlGate>,
) -> Vec<(String, MemberOutcome)> {
    let mut report = Vec::with_capacity(stack.members.len());
    let mut failed = false;
    for member in &stack.members {
        if failed {
            report.push((member.name.clone(), MemberOutcome::Skipped));
            continue;
        }
        let version = versions.get(&member.name).map(String::as_str);
        let outcome = match deployer
            .deploy(member, version, gates.get(&member.name))
            .await
        {
            Ok(output) if output.exit_code == 0 => MemberOutcome::Deployed,
            Ok(output) => MemberOutcome::Failed(output.pretty.trim().to_owned()),
            Err(error) => MemberOutcome::Failed(crate::commands::error_detail(&error)),
        };
        failed = matches!(outcome, MemberOutcome::Failed(_));
        report.push((member.name.clone(), outcome));
    }
    report
}

/// `name=version` pairs, checked against the stack's members.
///
/// # Errors
/// A pair without `=`, a name that is no member, or a member named twice.
pub(crate) fn parse_versions(
    stack: &Stack,
    pairs: &[String],
) -> Result<BTreeMap<String, String>, String> {
    let mut versions = BTreeMap::new();
    for pair in pairs {
        let Some((name, version)) = pair.split_once('=') else {
            return Err(format!(
                "--app-version takes <member>=<version>, not '{pair}'"
            ));
        };
        if !stack.members.iter().any(|m| m.name == name) {
            let known: Vec<&str> = stack.members.iter().map(|m| m.name.as_str()).collect();
            return Err(format!(
                "--app-version names '{name}', which is not a member (members: {})",
                known.join(", ")
            ));
        }
        if versions
            .insert(name.to_owned(), version.to_owned())
            .is_some()
        {
            return Err(format!("--app-version names '{name}' twice"));
        }
    }
    Ok(versions)
}

/// `stack deploy`: load and validate the stack, then deploy its members in
/// order. Exits 1 when the stack is invalid or a member failed.
pub(crate) async fn stack_deploy(
    stack_path: &Path,
    versions: &[String],
    deployer: &Deployer,
) -> Result<CommandOutput> {
    stack_deploy_with(
        stack_path,
        versions,
        deployer,
        Arc::new(crate::stack_sdl::LiveSdl),
        deployer.dry_run,
    )
    .await
}

fn refused(message: &str) -> CommandOutput {
    CommandOutput {
        exit_code: 1,
        pretty: format!("{message}\n"),
        json: json!({ "ok": false, "error": message }),
    }
}

/// Where the subgraphs' SDL is checked against, decided before any deploys.
enum Candidate {
    /// No member has `sdl`.
    None,
    /// Not checked, on purpose: why.
    NotChecked(String),
    /// The router's staged release, a local directory.
    Staged(PathBuf),
    /// The members deploy, but their SDL cannot be checked: why.
    Unreadable(String),
    /// Staging the router failed, so nothing deploys: why.
    Failed(String),
}

/// Stage the router's candidate release, unless nothing is checked, this is a
/// dry run, or the router deploys through `[hosts]`.
async fn candidate(
    stack: &Stack,
    versions: &BTreeMap<String, String>,
    deployer: &dyn MemberDeployer,
    dry_run: bool,
) -> Candidate {
    let router = stack
        .router
        .as_deref()
        .and_then(|router| stack.members.iter().find(|m| m.name == router));
    let Some(router) = router.filter(|_| stack.members.iter().any(|m| m.sdl.is_some())) else {
        return Candidate::None;
    };
    if dry_run {
        return Candidate::NotChecked("dry run".to_owned());
    }
    if router.config.hosts.is_some() {
        return Candidate::Unreadable(crate::stack_sdl::on_hosts(&router.name));
    }
    let version = versions.get(&router.name).map(String::as_str);
    match deployer.stage(router, version).await {
        Err(error) => Candidate::Failed(crate::commands::error_detail(&error)),
        Ok(path) if path.is_dir() => Candidate::Staged(path),
        Ok(path) => Candidate::Unreadable(format!(
            "the router '{}' stages its release as {}, not a directory, so it holds no SDL file",
            router.name,
            path.display()
        )),
    }
}

/// [`stack_deploy`] with the deployer supplied by the caller.
pub(crate) async fn stack_deploy_with(
    stack_path: &Path,
    versions: &[String],
    deployer: &dyn MemberDeployer,
    sdl: Arc<dyn SdlSource>,
    dry_run: bool,
) -> Result<CommandOutput> {
    use crate::stack_sdl::{SdlFinding, Verdict};

    let stack = match Stack::load(stack_path) {
        Ok(stack) => stack,
        Err(error) => return Ok(refused(&error.to_string())),
    };
    let versions = match parse_versions(&stack, versions) {
        Ok(versions) => versions,
        Err(message) => return Ok(refused(&message)),
    };
    let candidate = candidate(&stack, &versions, deployer, dry_run).await;
    let gates: BTreeMap<String, SdlGate> = match &candidate {
        Candidate::Staged(release) => stack
            .members
            .iter()
            .filter_map(|m| Some((m.name.clone(), SdlGate::new(m, release, Arc::clone(&sdl))?)))
            .collect(),
        _ => BTreeMap::new(),
    };
    let (report, stopped) = if let Candidate::Failed(why) = &candidate {
        let skipped = stack
            .members
            .iter()
            .map(|m| (m.name.clone(), MemberOutcome::Skipped))
            .collect();
        (
            skipped,
            Some(format!(
                "the router's candidate release could not be staged, so nothing deployed: {}",
                fraisier_saga::redact::credentials(why)
            )),
        )
    } else {
        (
            deploy_members(&stack, &versions, deployer, &gates).await,
            None,
        )
    };
    let all_deployed = report
        .iter()
        .all(|(_, outcome)| *outcome == MemberOutcome::Deployed);
    // A member that never reached its health step has no SDL finding.
    let findings: Vec<SdlFinding> = stack
        .members
        .iter()
        .zip(&report)
        .filter(|(member, _)| member.sdl.is_some())
        .filter_map(|(member, (_, outcome))| {
            let verdict = match &candidate {
                Candidate::Staged(_) => return gates.get(&member.name)?.finding(),
                Candidate::NotChecked(why) => Verdict::NotChecked(why.clone()),
                Candidate::Unreadable(why) if *outcome == MemberOutcome::Deployed => {
                    Verdict::Unchecked(why.clone())
                }
                _ => return None,
            };
            Some(SdlFinding {
                member: member.name.clone(),
                verdict,
            })
        })
        .collect();
    let (sdl_pretty, sdl_ok) = crate::stack_sdl::render(&findings);
    let ok = all_deployed && sdl_ok;
    let mut pretty = format!("stack {} ({}):\n", stack.name, stack.environment);
    let mut members = Vec::new();
    for (name, outcome) in &report {
        let (tag, detail) = match outcome {
            MemberOutcome::Deployed => ("deployed", None),
            MemberOutcome::Failed(why) => ("failed", Some(fraisier_saga::redact::credentials(why))),
            MemberOutcome::Skipped => ("skipped", None),
        };
        let _ = write!(pretty, "  {tag:<9} {name}");
        if let Some(detail) = &detail {
            let _ = write!(pretty, ": {}", detail.lines().next().unwrap_or_default());
        }
        pretty.push('\n');
        members.push(json!({ "name": name, "outcome": tag, "detail": detail }));
    }
    if let Some(why) = &stopped {
        let _ = writeln!(pretty, "  {why}");
    } else if !all_deployed {
        pretty.push_str(
            "  members before the failure stay deployed; the failed member rolled itself back\n",
        );
    }
    pretty.push_str(&sdl_pretty);
    Ok(CommandOutput {
        exit_code: i32::from(!ok),
        pretty,
        json: json!({
            "stack": stack.name,
            "environment": stack.environment,
            "ok": ok,
            "members": members,
            "error": stopped,
            "sdl": crate::stack_sdl::to_json(&findings),
        }),
    })
}

/// The real deployer: `fraisier deploy` for each member.
pub(crate) struct Deployer {
    /// The state store directory every member shares.
    pub(crate) state_dir: PathBuf,
    /// `--dry-run`.
    pub(crate) dry_run: bool,
    /// `--skip-preflight`.
    pub(crate) skip_preflight: bool,
}

#[async_trait]
impl MemberDeployer for Deployer {
    async fn deploy(
        &self,
        member: &Member,
        app_version: Option<&str>,
        gate: Option<&SdlGate>,
    ) -> Result<CommandOutput> {
        let guard = gate.map(|gate| move |health| gate.guard(health));
        crate::commands::deploy_guarded(
            &member.config_path,
            &self.state_dir,
            None,
            app_version,
            (self.dry_run, self.skip_preflight, false),
            guard
                .as_ref()
                .map(|g| g as crate::commands::HealthGuard<'_>),
        )
        .await
    }

    /// Stage through the router's own artifact adapter, as its deploy's fetch
    /// step will: its deploy stages the same version again before activating.
    async fn stage(&self, router: &Member, app_version: Option<&str>) -> Result<PathBuf> {
        let plan = crate::factory::build_artifact_probe_at(&router.config, app_version)?;
        let (host, _) = plan
            .hosts
            .first()
            .cloned()
            .context("the router has no host to stage on")?;
        let mut ctx = plan.ctx;
        ctx.host = Some(host.clone());
        Ok(plan.artifact.stage(&ctx, &host).await?.path)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use fraisier_config::stack::{Member, Stack, StackFile};
    use fraisier_core::adapter_axes::{
        AdapterCtx, AdapterError, HealthAdapter, HealthStatus, HostId,
    };

    use super::{deploy_members, parse_versions, stack_deploy_with, MemberDeployer, MemberOutcome};
    use crate::commands::CommandOutput;
    use crate::stack_sdl::SdlGate;
    use crate::test_env::block_on;

    /// Records each stage and deploy; the members named in `fail` fail, and so
    /// does a member whose gate finds it unhealthy, as its saga would.
    #[derive(Default)]
    struct Fake {
        deployed: Mutex<Vec<String>>,
        fail: Vec<&'static str>,
        /// What staging the router answers; `None` fails it.
        candidate: Option<PathBuf>,
    }

    /// The member's own health check: always up.
    struct Healthy;

    #[async_trait]
    impl HealthAdapter for Healthy {
        async fn check(&self, _: &AdapterCtx, _: &HostId) -> Result<HealthStatus, AdapterError> {
            Ok(HealthStatus {
                healthy: true,
                detail: None,
            })
        }
    }

    #[async_trait]
    impl MemberDeployer for Fake {
        async fn deploy(
            &self,
            member: &Member,
            app_version: Option<&str>,
            gate: Option<&SdlGate>,
        ) -> anyhow::Result<CommandOutput> {
            self.deployed.lock().expect("log").push(format!(
                "{}@{}",
                member.name,
                app_version.unwrap_or("-")
            ));
            let unhealthy = match gate {
                None => None,
                Some(gate) => {
                    let status = gate
                        .guard(Arc::new(Healthy))
                        .check(
                            &AdapterCtx::new(member.name.clone(), "staging"),
                            &HostId::new("localhost"),
                        )
                        .await?;
                    (!status.healthy).then(|| status.detail.unwrap_or_default())
                }
            };
            let failed = self.fail.contains(&member.name.as_str()) || unhealthy.is_some();
            let why = unhealthy.unwrap_or_else(|| "postgres://u:hunter2@db/x".to_owned());
            Ok(CommandOutput {
                exit_code: i32::from(failed),
                pretty: if failed {
                    format!(
                        "deploy of {}/staging rolled_back (step 'health': {why})\n",
                        member.name
                    )
                } else {
                    format!("deploy of {}/staging committed\n", member.name)
                },
                json: serde_json::json!({}),
            })
        }

        async fn stage(
            &self,
            router: &Member,
            app_version: Option<&str>,
        ) -> anyhow::Result<PathBuf> {
            self.deployed.lock().expect("log").push(format!(
                "stage {}@{}",
                router.name,
                app_version.unwrap_or("-")
            ));
            self.candidate
                .clone()
                .ok_or_else(|| anyhow::anyhow!("download of the router release failed"))
        }
    }

    const STACK: &str = "[stack]\nname = \"erp\"\nenvironment = \"staging\"\n\n\
                         [[stack.member]]\nconfig = \"router.toml\"\ndepends_on = [\"billing\", \"management\"]\n\n\
                         [[stack.member]]\nconfig = \"management.toml\"\n\n\
                         [[stack.member]]\nconfig = \"billing.toml\"\n";

    fn member_toml(name: &str) -> String {
        format!("[deploy]\nname = \"{name}\"\nenvironment = \"staging\"\n")
    }

    fn stack() -> Stack {
        let file = StackFile::from_toml_str(STACK).expect("parses");
        Stack::resolve(&file, Path::new("/s"), &|path| {
            let name = path
                .file_stem()
                .expect("stem")
                .to_string_lossy()
                .into_owned();
            Ok(member_toml(&name))
        })
        .expect("valid")
    }

    #[test]
    fn members_deploy_in_order() {
        let fake = Fake::default();
        let report = block_on(deploy_members(
            &stack(),
            &BTreeMap::new(),
            &fake,
            &BTreeMap::new(),
        ));
        assert_eq!(
            *fake.deployed.lock().expect("log"),
            vec!["billing@-", "management@-", "router@-"]
        );
        assert!(report.iter().all(|(_, o)| *o == MemberOutcome::Deployed));
    }

    #[test]
    fn a_failed_member_stops_the_ones_after_it() {
        let fake = Fake {
            fail: vec!["management"],
            ..Fake::default()
        };
        let report = block_on(deploy_members(
            &stack(),
            &BTreeMap::new(),
            &fake,
            &BTreeMap::new(),
        ));
        assert_eq!(
            *fake.deployed.lock().expect("log"),
            vec!["billing@-", "management@-"],
            "the router never starts"
        );
        assert_eq!(report[0], ("billing".to_owned(), MemberOutcome::Deployed));
        assert!(matches!(&report[1], (name, MemberOutcome::Failed(why))
            if name == "management" && why.contains("rolled_back")));
        assert_eq!(report[2], ("router".to_owned(), MemberOutcome::Skipped));
    }

    #[test]
    fn versions_reach_their_members() {
        let fake = Fake::default();
        let versions = parse_versions(&stack(), &["router=v3".to_owned(), "billing=v1".to_owned()])
            .expect("valid");
        block_on(deploy_members(&stack(), &versions, &fake, &BTreeMap::new()));
        assert_eq!(
            *fake.deployed.lock().expect("log"),
            vec!["billing@v1", "management@-", "router@v3"]
        );
    }

    #[test]
    fn a_version_for_no_member_or_without_a_name_is_refused() {
        assert!(parse_versions(&stack(), &["ghost=v1".to_owned()])
            .expect_err("unknown")
            .contains("ghost"));
        assert!(parse_versions(&stack(), &["v1".to_owned()]).is_err());
        assert!(
            parse_versions(&stack(), &["router=v1".to_owned(), "router=v2".to_owned()]).is_err()
        );
    }

    /// Answers every SDL query, the live SDL differing when `differs` is set;
    /// records each release file read and each query.
    #[derive(Default)]
    struct Sdl {
        differs: bool,
        read: Mutex<Vec<PathBuf>>,
        asked: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl crate::stack_sdl::SdlSource for Sdl {
        async fn live_sdl(&self, url: &str, _: &[(String, String)]) -> Result<String, String> {
            self.asked.lock().expect("asked").push(url.to_owned());
            Ok(if self.differs {
                "type Query { b: Int }"
            } else {
                "type Query { a: Int }"
            }
            .to_owned())
        }
        fn served_sdl(&self, path: &Path) -> Result<String, String> {
            self.read.lock().expect("read").push(path.to_owned());
            Ok("type Query { a: Int }".to_owned())
        }
    }

    fn sdl(differs: bool) -> Arc<Sdl> {
        Arc::new(Sdl {
            differs,
            ..Sdl::default()
        })
    }

    /// [`stack_dir`], with `router` as the router (`router_extra` appended to
    /// its config) and `management` checked.
    fn routed_stack_dir(dir: &Path, router_extra: &str) -> PathBuf {
        let path = stack_dir(dir);
        std::fs::write(
            dir.join("router.toml"),
            format!(
                "{}[artifact]\nsource = \"local\"\nactive_path = \"/srv/router/current\"\n\
                 {router_extra}",
                member_toml("router")
            ),
        )
        .expect("write");
        std::fs::write(
            dir.join("management.toml"),
            format!(
                "{}[federation]\ngraphql_url = \"http://management/graphql\"\n",
                member_toml("management")
            ),
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
                    "config = \"management.toml\"\n",
                    "config = \"management.toml\"\nsdl = \"sdl/management.graphql\"\n",
                ),
        )
        .expect("write");
        path
    }

    /// A staged router release: a directory.
    fn candidate(dir: &Path) -> PathBuf {
        let candidate = dir.join("releases/v3");
        std::fs::create_dir_all(&candidate).expect("mkdir");
        candidate
    }

    #[test]
    fn a_subgraph_its_candidate_router_release_does_not_hold_fails_and_the_router_never_deploys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = routed_stack_dir(dir.path(), "");
        let fake = Fake {
            candidate: Some(candidate(dir.path())),
            ..Fake::default()
        };
        let source = sdl(true);
        let out = block_on(stack_deploy_with(
            &path,
            &["router=v3".to_owned()],
            &fake,
            source.clone(),
            false,
        ))
        .expect("runs");
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert_eq!(
            *fake.deployed.lock().expect("log"),
            vec!["stage router@v3", "billing@-", "management@-"],
            "the candidate is staged first; the router never deploys"
        );
        assert_eq!(
            *source.read.lock().expect("read"),
            vec![candidate(dir.path()).join("sdl/management.graphql")],
            "the SDL is read from the candidate, not the active release"
        );
        assert!(
            out.pretty.contains(
                "failed    management: deploy of management/staging rolled_back (step 'health'"
            ),
            "{}",
            out.pretty
        );
        assert!(out.pretty.contains("skipped   router"), "{}", out.pretty);
        assert!(
            out.pretty.contains("  sdl management: DIFFERS"),
            "{}",
            out.pretty
        );
        assert_eq!(out.json["sdl"][0]["matches"], serde_json::json!(false));
        assert_eq!(
            out.json["members"][2]["outcome"],
            serde_json::json!("skipped")
        );
    }

    #[test]
    fn subgraphs_that_match_the_candidate_deploy_before_the_router() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = routed_stack_dir(dir.path(), "");
        let fake = Fake {
            candidate: Some(candidate(dir.path())),
            ..Fake::default()
        };
        let out = block_on(stack_deploy_with(
            &path,
            &["router=v3".to_owned()],
            &fake,
            sdl(false),
            false,
        ))
        .expect("runs");
        assert_eq!(out.exit_code, 0, "{}", out.pretty);
        assert_eq!(
            *fake.deployed.lock().expect("log"),
            vec!["stage router@v3", "billing@-", "management@-", "router@v3"]
        );
        assert!(
            out.pretty.contains("  sdl management: matches"),
            "{}",
            out.pretty
        );
    }

    #[test]
    fn a_dry_run_stages_nothing_asks_no_sdl_and_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = routed_stack_dir(dir.path(), "");
        let fake = Fake::default();
        let source = sdl(true);
        let out =
            block_on(stack_deploy_with(&path, &[], &fake, source.clone(), true)).expect("runs");
        assert_eq!(out.exit_code, 0, "{}", out.pretty);
        assert_eq!(
            *fake.deployed.lock().expect("log"),
            vec!["billing@-", "management@-", "router@-"]
        );
        assert!(source.asked.lock().expect("asked").is_empty());
        assert!(source.read.lock().expect("read").is_empty());
        assert!(
            out.pretty
                .contains("  sdl management: not checked (dry run)"),
            "{}",
            out.pretty
        );
        assert_eq!(out.json["sdl"][0]["skipped"], serde_json::json!("dry run"));
    }

    #[test]
    fn a_router_on_hosts_is_unchecked_never_green() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = routed_stack_dir(
            dir.path(),
            "[hosts]\nstrategy = \"rolling\"\n\
             inventory = [{ name = \"gw-1\", address = \"10.0.0.1\" }]\n",
        );
        let fake = Fake::default();
        let source = sdl(false);
        let out =
            block_on(stack_deploy_with(&path, &[], &fake, source.clone(), false)).expect("runs");
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert_eq!(
            *fake.deployed.lock().expect("log"),
            vec!["billing@-", "management@-", "router@-"],
            "nothing is staged locally for a router on its hosts"
        );
        assert!(source.read.lock().expect("read").is_empty());
        assert!(
            out.pretty.contains(
                "  sdl management: UNCHECKED\n      the router 'router' deploys through [hosts]"
            ),
            "{}",
            out.pretty
        );
    }

    #[test]
    fn a_router_release_that_cannot_be_staged_deploys_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = routed_stack_dir(dir.path(), "");
        let fake = Fake::default();
        let out = block_on(stack_deploy_with(&path, &[], &fake, sdl(false), false)).expect("runs");
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert_eq!(*fake.deployed.lock().expect("log"), vec!["stage router@-"]);
        assert!(
            out.pretty.contains(
                "the router's candidate release could not be staged, so nothing deployed: \
                 download of the router release failed"
            ),
            "{}",
            out.pretty
        );
        assert!(out.pretty.contains("skipped   billing"), "{}", out.pretty);
    }

    #[test]
    fn a_router_release_that_is_not_a_directory_is_unchecked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = routed_stack_dir(dir.path(), "");
        let file = dir.path().join("router-v3.tar.gz");
        std::fs::write(&file, "bytes").expect("write");
        let fake = Fake {
            candidate: Some(file),
            ..Fake::default()
        };
        let out = block_on(stack_deploy_with(&path, &[], &fake, sdl(false), false)).expect("runs");
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(
            out.pretty.contains("  sdl management: UNCHECKED"),
            "{}",
            out.pretty
        );
        assert!(out.pretty.contains("not a directory"), "{}", out.pretty);
    }

    #[test]
    fn a_failed_stack_deploy_checks_no_sdl() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = routed_stack_dir(dir.path(), "");
        let fake = Fake {
            fail: vec!["billing"],
            candidate: Some(candidate(dir.path())),
            ..Fake::default()
        };
        let out = block_on(stack_deploy_with(&path, &[], &fake, sdl(true), false)).expect("runs");
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(!out.pretty.contains("sdl"), "{}", out.pretty);
    }

    #[test]
    fn the_real_deployer_stages_the_router_release_without_activating_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let build = dir.path().join("build");
        std::fs::create_dir_all(build.join("sdl")).expect("mkdir");
        std::fs::write(build.join("sdl/billing.graphql"), "type Query { a: Int }").expect("sdl");
        let config = format!(
            "{}[artifact]\nsource = \"local\"\npath = \"{}\"\nactive_path = \"{}\"\n\
             staging_dir = \"{}\"\n",
            member_toml("router"),
            build.display(),
            dir.path().join("current").display(),
            dir.path().join("releases").display()
        );
        let router = Member {
            name: "router".to_owned(),
            config_path: dir.path().join("router.toml"),
            config: fraisier_config::DeployConfig::from_toml_str(&config).expect("parses"),
            depends_on: Vec::new(),
            sdl: None,
        };
        let deployer = super::Deployer {
            state_dir: dir.path().join("state"),
            dry_run: false,
            skip_preflight: false,
        };
        let staged = block_on(deployer.stage(&router, Some("v3"))).expect("staged");
        assert_eq!(staged, dir.path().join("releases/v3"));
        assert!(staged.join("sdl/billing.graphql").is_file());
        assert!(
            !dir.path().join("current").exists(),
            "staging activates nothing"
        );
    }

    fn stack_dir(dir: &Path) -> std::path::PathBuf {
        for name in ["router", "management", "billing"] {
            std::fs::write(dir.join(format!("{name}.toml")), member_toml(name)).expect("write");
        }
        let path = dir.join("stack.staging.toml");
        std::fs::write(&path, STACK).expect("write");
        path
    }

    #[test]
    fn the_command_reports_each_member_and_fails_when_one_did() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = stack_dir(dir.path());
        let fake = Fake {
            fail: vec!["management"],
            ..Fake::default()
        };
        let out = block_on(stack_deploy_with(&path, &[], &fake, sdl(false), false)).expect("runs");
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(out.pretty.contains("deployed  billing"), "{}", out.pretty);
        assert!(
            out.pretty.contains("failed    management"),
            "{}",
            out.pretty
        );
        assert!(out.pretty.contains("skipped   router"), "{}", out.pretty);
        assert!(!out.pretty.contains("hunter2"), "{}", out.pretty);
        assert_eq!(out.json["ok"], serde_json::json!(false));
        assert_eq!(
            out.json["members"][2]["outcome"],
            serde_json::json!("skipped")
        );
        assert!(!out.json.to_string().contains("hunter2"));

        let out = block_on(stack_deploy_with(
            &path,
            &[],
            &Fake::default(),
            sdl(false),
            false,
        ))
        .expect("runs");
        assert_eq!(out.exit_code, 0, "{}", out.pretty);
    }

    #[test]
    fn an_invalid_stack_deploys_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = stack_dir(dir.path());
        std::fs::remove_file(dir.path().join("billing.toml")).expect("rm");
        let fake = Fake::default();
        let out = block_on(stack_deploy_with(&path, &[], &fake, sdl(false), false)).expect("runs");
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(out.pretty.contains("billing.toml"), "{}", out.pretty);
        assert!(fake.deployed.lock().expect("log").is_empty());
    }
}
