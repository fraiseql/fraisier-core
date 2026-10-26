//! `fraisier stack deploy`: a stack's members, deployed one after another in
//! dependency order.
//!
//! Each member deploys through the same path `fraisier deploy` takes, with its
//! own config, saga and rollback. There is no rollback across members: a member
//! that fails rolls itself back, the members after it never start, and the
//! members before it stay deployed. The report says which is which.
//!
//! Once every member deployed, each subgraph's live SDL is checked against
//! the one its router serves (see [`crate::stack_sdl`]); a difference fails the
//! command, though everything stays deployed.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use fraisier_config::stack::{Member, Stack};
use serde_json::json;

use crate::commands::CommandOutput;

/// Deploys one member.
#[async_trait]
pub(crate) trait MemberDeployer: Send + Sync {
    /// Deploy `member` at `app_version`, as `fraisier deploy` would.
    async fn deploy(&self, member: &Member, app_version: Option<&str>) -> Result<CommandOutput>;
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
) -> Vec<(String, MemberOutcome)> {
    let mut report = Vec::with_capacity(stack.members.len());
    let mut failed = false;
    for member in &stack.members {
        if failed {
            report.push((member.name.clone(), MemberOutcome::Skipped));
            continue;
        }
        let version = versions.get(&member.name).map(String::as_str);
        let outcome = match deployer.deploy(member, version).await {
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
        &crate::stack_sdl::LiveSdl,
        &|name| std::env::var(name).ok(),
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

/// [`stack_deploy`] with the deployer supplied by the caller.
pub(crate) async fn stack_deploy_with(
    stack_path: &Path,
    versions: &[String],
    deployer: &dyn MemberDeployer,
    sdl: &dyn crate::stack_sdl::SdlSource,
    env: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Result<CommandOutput> {
    let stack = match Stack::load(stack_path) {
        Ok(stack) => stack,
        Err(error) => return Ok(refused(&error.to_string())),
    };
    let versions = match parse_versions(&stack, versions) {
        Ok(versions) => versions,
        Err(message) => return Ok(refused(&message)),
    };
    let report = deploy_members(&stack, &versions, deployer).await;
    let all_deployed = report
        .iter()
        .all(|(_, outcome)| *outcome == MemberOutcome::Deployed);
    // Once, at the end: mid-stack, a subgraph ahead of its router is expected.
    let findings = if all_deployed {
        crate::stack_sdl::check(&stack, sdl, env).await
    } else {
        Vec::new()
    };
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
    if !all_deployed {
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
    async fn deploy(&self, member: &Member, app_version: Option<&str>) -> Result<CommandOutput> {
        crate::commands::deploy(
            &member.config_path,
            &self.state_dir,
            None,
            app_version,
            self.dry_run,
            self.skip_preflight,
            false,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use fraisier_config::stack::{Member, Stack, StackFile};

    use super::{deploy_members, parse_versions, stack_deploy_with, MemberDeployer, MemberOutcome};
    use crate::commands::CommandOutput;
    use crate::test_env::block_on;

    /// Records each deploy; the members named in `fail` fail.
    #[derive(Default)]
    struct Fake {
        deployed: Mutex<Vec<String>>,
        fail: Vec<&'static str>,
    }

    #[async_trait]
    impl MemberDeployer for Fake {
        async fn deploy(
            &self,
            member: &Member,
            app_version: Option<&str>,
        ) -> anyhow::Result<CommandOutput> {
            self.deployed.lock().expect("log").push(format!(
                "{}@{}",
                member.name,
                app_version.unwrap_or("-")
            ));
            let failed = self.fail.contains(&member.name.as_str());
            Ok(CommandOutput {
                exit_code: i32::from(failed),
                pretty: if failed {
                    format!(
                        "deploy of {}/staging rolled_back (step 'health': postgres://u:hunter2@db/x)\n",
                        member.name
                    )
                } else {
                    format!("deploy of {}/staging committed\n", member.name)
                },
                json: serde_json::json!({}),
            })
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
        let report = block_on(deploy_members(&stack(), &BTreeMap::new(), &fake));
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
        let report = block_on(deploy_members(&stack(), &BTreeMap::new(), &fake));
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
        block_on(deploy_members(&stack(), &versions, &fake));
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

    /// Answers every SDL query: the live SDL differs when `.0` is set.
    struct Sdl(bool);

    #[async_trait]
    impl crate::stack_sdl::SdlSource for Sdl {
        async fn live_sdl(&self, _: &str, _: &[(String, String)]) -> Result<String, String> {
            Ok(if self.0 {
                "type Query { b: Int }"
            } else {
                "type Query { a: Int }"
            }
            .to_owned())
        }
        fn served_sdl(&self, _: &Path) -> Result<String, String> {
            Ok("type Query { a: Int }".to_owned())
        }
    }

    /// [`stack_dir`], with `router` as the router and `management` checked.
    fn routed_stack_dir(dir: &Path) -> std::path::PathBuf {
        let path = stack_dir(dir);
        std::fs::write(
            dir.join("router.toml"),
            format!(
                "{}[artifact]\nsource = \"local\"\nactive_path = \"/srv/router/current\"\n",
                member_toml("router")
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
                    "config = \"management.toml\"\nsdl = \"sdl/management.graphql\"\n\
                     graphql_url = \"http://management/graphql\"\n",
                ),
        )
        .expect("write");
        path
    }

    #[test]
    fn the_sdl_is_checked_once_every_member_deployed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = routed_stack_dir(dir.path());
        let out = block_on(stack_deploy_with(
            &path,
            &[],
            &Fake::default(),
            &Sdl(true),
            &|_| None,
        ))
        .expect("runs");
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(
            out.pretty.contains("  sdl management: DIFFERS"),
            "{}",
            out.pretty
        );
        assert_eq!(out.json["ok"], serde_json::json!(false));
        assert_eq!(out.json["sdl"][0]["matches"], serde_json::json!(false));

        let out = block_on(stack_deploy_with(
            &path,
            &[],
            &Fake::default(),
            &Sdl(false),
            &|_| None,
        ))
        .expect("runs");
        assert_eq!(out.exit_code, 0, "{}", out.pretty);
        assert!(
            out.pretty.contains("  sdl management: matches"),
            "{}",
            out.pretty
        );
    }

    #[test]
    fn a_failed_stack_deploy_checks_no_sdl() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = routed_stack_dir(dir.path());
        let fake = Fake {
            fail: vec!["billing"],
            ..Fake::default()
        };
        let out =
            block_on(stack_deploy_with(&path, &[], &fake, &Sdl(true), &|_| None)).expect("runs");
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(!out.pretty.contains("sdl"), "{}", out.pretty);
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
        let out =
            block_on(stack_deploy_with(&path, &[], &fake, &Sdl(false), &|_| None)).expect("runs");
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
            &Sdl(false),
            &|_| None,
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
        let out =
            block_on(stack_deploy_with(&path, &[], &fake, &Sdl(false), &|_| None)).expect("runs");
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(out.pretty.contains("billing.toml"), "{}", out.pretty);
        assert!(fake.deployed.lock().expect("log").is_empty());
    }
}
