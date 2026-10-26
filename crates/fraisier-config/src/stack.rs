//! The stack file: one environment's deployables, deployed as one stack.
//!
//! A `stack.<env>.toml` references each member's own `fraisier.toml` by path,
//! with the members it depends on:
//!
//! ```toml
//! [stack]
//! name = "erp"
//! environment = "staging"
//!
//! [[stack.member]]
//! config = "management.staging.toml"
//!
//! [[stack.member]]
//! config = "gateway.staging.toml"
//! depends_on = ["management"]
//! ```
//!
//! A member's name is its config's `[deploy].name`, never restated here, so the
//! two cannot disagree. Every member config's `[deploy].environment` must be
//! the stack's: a production config cannot sit in a staging stack.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::DeployConfig;

/// A parsed stack file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackFile {
    /// The `[stack]` section.
    pub stack: StackSection,
}

/// The `[stack]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackSection {
    /// The stack's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The environment every member deploys to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// The member (by `[deploy].name`) that is the stack's router: the release
    /// every member's `sdl` path is read from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub router: Option<String>,
    /// The members, in any order.
    #[serde(default, rename = "member", skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<MemberSpec>,
}

/// One `[[stack.member]]` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberSpec {
    /// The member's `fraisier.toml`, relative to the stack file.
    pub config: PathBuf,
    /// The members (by `[deploy].name`) that must deploy first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// The member's subgraph SDL as the router serves it: a path inside the
    /// router member's release. The endpoint it is compared with is the member
    /// config's `[federation].graphql_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdl: Option<PathBuf>,
}

/// A member, resolved: its config loaded and its name derived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The member's `[deploy].name`.
    pub name: String,
    /// The member's config file, resolved against the stack file's directory.
    pub config_path: PathBuf,
    /// The member's parsed config.
    pub config: DeployConfig,
    /// The members it depends on.
    pub depends_on: Vec<String>,
    /// See [`MemberSpec::sdl`].
    pub sdl: Option<PathBuf>,
}

impl Member {
    /// The endpoint the SDL check asks: the config's `[federation].graphql_url`.
    #[must_use]
    pub fn graphql_url(&self) -> Option<&str> {
        self.config
            .federation
            .as_ref()
            .and_then(|f| f.graphql_url.as_deref())
    }
}

/// A validated stack, its members in deploy order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack {
    /// The stack's name.
    pub name: String,
    /// The environment every member deploys to.
    pub environment: String,
    /// See [`StackSection::router`].
    pub router: Option<String>,
    /// The members, each after everything it depends on.
    pub members: Vec<Member>,
}

/// Why a stack file is not a stack: every problem found, one per line.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid stack file:\n  {}", .0.join("\n  "))]
pub struct StackError(pub Vec<String>);

impl StackFile {
    /// Parse a stack file.
    ///
    /// # Errors
    /// The TOML does not parse, has an unknown field, or restates a member
    /// property that lives in the member's config.
    pub fn from_toml_str(toml: &str) -> Result<Self, StackError> {
        let moved = moved_keys(toml);
        if !moved.is_empty() {
            return Err(StackError(moved));
        }
        toml::from_str(toml).map_err(|error| StackError(vec![error.to_string()]))
    }
}

impl Stack {
    /// Load the stack file at `path` and every member config it references.
    ///
    /// # Errors
    /// Every problem the file and its members have, together.
    pub fn load(path: &Path) -> Result<Self, StackError> {
        let toml = std::fs::read_to_string(path).map_err(|error| {
            StackError(vec![format!("cannot read {}: {error}", path.display())])
        })?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        Self::resolve(&StackFile::from_toml_str(&toml)?, base, &|member| {
            std::fs::read_to_string(member).map_err(|error| error.to_string())
        })
    }

    /// Resolve `file`'s members against `base`, reading each config with `read`.
    ///
    /// # Errors
    /// Every problem found, together.
    pub fn resolve(
        file: &StackFile,
        base: &Path,
        read: &dyn Fn(&Path) -> Result<String, String>,
    ) -> Result<Self, StackError> {
        let mut problems = Vec::new();
        let section = &file.stack;
        let name = section.name.clone().filter(|n| !n.trim().is_empty());
        let environment = section.environment.clone().filter(|e| !e.trim().is_empty());
        if name.is_none() {
            problems.push("[stack].name is required".to_owned());
        }
        if environment.is_none() {
            problems.push("[stack].environment is required".to_owned());
        }
        if section.members.is_empty() {
            problems.push("a stack needs at least one [[stack.member]]".to_owned());
        }

        let mut members = Vec::new();
        for spec in &section.members {
            let config_path = base.join(&spec.config);
            let shown = spec.config.display();
            let config = match read(&config_path) {
                Err(error) => {
                    problems.push(format!(
                        "member {shown}: cannot read {}: {error}",
                        config_path.display()
                    ));
                    continue;
                }
                Ok(toml) => match DeployConfig::from_toml_str(&toml) {
                    Ok(config) => config,
                    Err(error) => {
                        problems.push(format!("member {shown}: {error}"));
                        continue;
                    }
                },
            };
            let deploy = config.deploy.as_ref();
            let Some(member_name) = deploy.and_then(|d| d.name.clone()) else {
                problems.push(format!(
                    "member {shown}: its config has no [deploy].name, which names the member"
                ));
                continue;
            };
            let member_environment = deploy.and_then(|d| d.environment.clone());
            if let Some(stack_environment) = environment.as_deref() {
                if member_environment.as_deref() != Some(stack_environment) {
                    problems.push(format!(
                        "member '{member_name}' ({shown}) deploys to '{}', not the stack's \
                         environment '{stack_environment}'",
                        member_environment.as_deref().unwrap_or("<none>")
                    ));
                }
            }
            members.push(Member {
                name: member_name,
                config_path,
                config,
                depends_on: spec.depends_on.clone(),
                sdl: spec.sdl.clone(),
            });
        }

        problems.extend(reference_problems(&members));
        problems.extend(sdl_problems(section.router.as_deref(), &members));
        if !problems.is_empty() {
            return Err(StackError(problems));
        }

        let graph: Vec<(String, Vec<String>)> = members
            .iter()
            .map(|m| (m.name.clone(), m.depends_on.clone()))
            .collect();
        let order = deploy_order(&graph).map_err(|cycle| {
            StackError(vec![format!(
                "members depend on each other in a cycle: {}",
                cycle.join(", ")
            )])
        })?;
        let mut slots: Vec<Option<Member>> = members.into_iter().map(Some).collect();
        let stack = Self {
            name: name.unwrap_or_default(),
            environment: environment.unwrap_or_default(),
            router: section.router.clone(),
            members: order
                .into_iter()
                .filter_map(|index| slots.get_mut(index).and_then(Option::take))
                .collect(),
        };
        let early = stack.subgraphs_after_the_router();
        if !early.is_empty() {
            return Err(StackError(early));
        }
        Ok(stack)
    }
}

/// Member keys a stack file once had, each with where it went.
const MOVED: [(&str, &str); 2] = [
    (
        "graphql_url",
        "graphql_url moved to the member's config, as [federation].graphql_url",
    ),
    (
        "sdl_headers",
        "sdl_headers is gone; the SDL query sends the member's [health] headers and \
         token_provider",
    ),
];

/// A pointer for every member that still sets a key in [`MOVED`]. Empty when
/// `toml` does not parse: the typed parse reports that.
fn moved_keys(toml: &str) -> Vec<String> {
    let Ok(file) = toml.parse::<toml::Table>() else {
        return Vec::new();
    };
    let members = file
        .get("stack")
        .and_then(|stack| stack.get("member"))
        .and_then(toml::Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let mut problems = Vec::new();
    for member in members {
        let shown = member
            .get("config")
            .and_then(toml::Value::as_str)
            .unwrap_or("<no config>");
        for (key, pointer) in MOVED {
            if member.get(key).is_some() {
                problems.push(format!("member {shown}: {pointer}"));
            }
        }
    }
    problems
}

impl Stack {
    /// Each member with `sdl` that deploys after the router. The router must
    /// come last: a subgraph whose SDL its candidate release does not hold
    /// fails before the router is committed.
    fn subgraphs_after_the_router(&self) -> Vec<String> {
        let Some(router) = self.router.as_deref() else {
            return Vec::new();
        };
        self.members
            .iter()
            .skip_while(|m| m.name != router)
            .filter(|m| m.sdl.is_some())
            .map(|m| {
                format!(
                    "the router '{router}' deploys before '{}', whose SDL its release holds: \
                     add '{}' to its depends_on",
                    m.name, m.name
                )
            })
            .collect()
    }
}

/// Duplicate member names, and dependencies that name no member.
fn reference_problems(members: &[Member]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut paths: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for member in members {
        paths
            .entry(member.name.as_str())
            .or_default()
            .push(member.config_path.display().to_string());
    }
    for (duplicate, at) in paths.iter().filter(|(_, at)| at.len() > 1) {
        problems.push(format!(
            "more than one member is named '{duplicate}': {}",
            at.join(", ")
        ));
    }
    for member in members {
        for dependency in &member.depends_on {
            if !paths.contains_key(dependency.as_str()) {
                problems.push(format!(
                    "member '{}' depends on '{dependency}', which is not a member",
                    member.name
                ));
            }
        }
    }
    problems
}

/// What the subgraph/supergraph check needs and lacks.
fn sdl_problems(router: Option<&str>, members: &[Member]) -> Vec<String> {
    let mut problems = Vec::new();
    let router_member = router.and_then(|name| members.iter().find(|m| m.name == name));
    if let (Some(name), None) = (router, router_member) {
        problems.push(format!("[stack].router '{name}' is not a member"));
    }
    let checked: Vec<&Member> = members.iter().filter(|m| m.sdl.is_some()).collect();
    for member in members {
        let name = &member.name;
        if member.sdl.is_some() && member.graphql_url().is_none() {
            problems.push(format!(
                "member '{name}' has sdl, but its config has no [federation].graphql_url"
            ));
        }
        // The check runs in the member's health step; a blue-green deploy
        // probes its green fleet instead, which the stack does not gate.
        let strategy = member.config.deploy.as_ref().and_then(|d| d.strategy.as_deref());
        if member.sdl.is_some() && strategy == Some("blue-green") {
            problems.push(format!(
                "member '{name}' has sdl, but deploys blue-green; the SDL check runs in a \
                 single- or multi-host deploy's health step"
            ));
        }
        if let Some(sdl) = &member.sdl {
            if sdl.is_absolute()
                || sdl
                    .components()
                    .any(|c| c == std::path::Component::ParentDir)
            {
                problems.push(format!(
                    "member '{name}': sdl must be a path inside the router's release, not {}",
                    sdl.display()
                ));
            }
            if router == Some(name.as_str()) {
                problems.push(format!(
                    "member '{name}' is the router, which serves the supergraph; it has no sdl"
                ));
            }
        }
    }
    if checked.is_empty() {
        return problems;
    }
    match (router, router_member) {
        (None, _) => problems.push(format!(
            "member(s) {} have sdl, a path in the router's release: name the router with \
             [stack].router",
            checked
                .iter()
                .map(|m| format!("'{}'", m.name))
                .collect::<Vec<_>>()
                .join(", ")
        )),
        (Some(name), Some(member))
            if member
                .config
                .artifact
                .as_ref()
                .and_then(|a| a.active_path.as_ref())
                .is_none() =>
        {
            problems.push(format!(
                "the router '{name}' has no [artifact].active_path, the release sdl paths are \
                 read from"
            ));
        }
        _ => {}
    }
    problems
}

/// The deploy order of `members` (each a name and what it depends on): every
/// member after its dependencies, ties broken by name. Indices into `members`.
///
/// # Errors
/// The members that form or hang off a cycle.
pub fn deploy_order(members: &[(String, Vec<String>)]) -> Result<Vec<usize>, Vec<String>> {
    let index: BTreeMap<&str, usize> = members
        .iter()
        .enumerate()
        .map(|(i, (name, _))| (name.as_str(), i))
        .collect();
    // Waiting on: each member's dependencies that are members and not yet placed.
    let mut waiting: Vec<BTreeSet<&str>> = members
        .iter()
        .map(|(_, on)| {
            on.iter()
                .map(String::as_str)
                .filter(|dependency| index.contains_key(dependency))
                .collect()
        })
        .collect();
    let mut ready: BTreeSet<&str> = members
        .iter()
        .zip(&waiting)
        .filter(|(_, on)| on.is_empty())
        .map(|((name, _), _)| name.as_str())
        .collect();
    let mut order = Vec::with_capacity(members.len());
    while let Some(next) = ready.pop_first() {
        order.push(index[next]);
        for (i, on) in waiting.iter_mut().enumerate() {
            if on.remove(next) && on.is_empty() {
                ready.insert(members[i].0.as_str());
            }
        }
    }
    if order.len() == members.len() {
        return Ok(order);
    }
    let mut stuck: Vec<String> = members
        .iter()
        .enumerate()
        .filter(|(i, _)| !order.contains(i))
        .map(|(_, (name, _))| name.clone())
        .collect();
    stuck.sort();
    Err(stuck)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use super::{deploy_order, Stack, StackFile};

    fn deps(list: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
        list.iter()
            .map(|(name, on)| {
                (
                    (*name).to_owned(),
                    on.iter().map(|d| (*d).to_owned()).collect(),
                )
            })
            .collect()
    }

    fn names(order: &[usize], members: &[(String, Vec<String>)]) -> Vec<String> {
        order.iter().map(|&i| members[i].0.clone()).collect()
    }

    #[test]
    fn dependencies_deploy_first_and_ties_break_by_name() {
        let members = deps(&[
            ("router", &["management", "billing"]),
            ("management", &["db"]),
            ("billing", &["db"]),
            ("db", &[]),
        ]);
        let order = deploy_order(&members).expect("acyclic");
        assert_eq!(
            names(&order, &members),
            vec!["db", "billing", "management", "router"]
        );
    }

    #[test]
    fn the_order_does_not_depend_on_the_files_order() {
        let a = deps(&[("b", &[]), ("a", &[]), ("c", &["a"])]);
        let b = deps(&[("c", &["a"]), ("a", &[]), ("b", &[])]);
        assert_eq!(
            names(&deploy_order(&a).expect("ok"), &a),
            names(&deploy_order(&b).expect("ok"), &b)
        );
        assert_eq!(
            names(&deploy_order(&a).expect("ok"), &a),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn a_cycle_names_its_members() {
        let members = deps(&[("a", &["b"]), ("b", &["a"]), ("c", &[])]);
        let cycle = deploy_order(&members).expect_err("a cycle");
        assert_eq!(cycle, vec!["a".to_owned(), "b".to_owned()]);
    }

    fn member(name: &str, environment: &str) -> String {
        format!("[deploy]\nname = \"{name}\"\nenvironment = \"{environment}\"\n")
    }

    fn resolve(stack: &str, files: &[(&str, String)]) -> Result<Stack, Vec<String>> {
        let files: BTreeMap<PathBuf, String> = files
            .iter()
            .map(|(path, body)| (Path::new("/stacks").join(path), body.clone()))
            .collect();
        let file = StackFile::from_toml_str(stack).map_err(|e| e.0)?;
        Stack::resolve(&file, Path::new("/stacks"), &|path| {
            files
                .get(path)
                .cloned()
                .ok_or_else(|| "No such file or directory".to_owned())
        })
        .map_err(|e| e.0)
    }

    const HEAD: &str = "[stack]\nname = \"erp\"\nenvironment = \"staging\"\n";

    #[test]
    fn a_stack_resolves_its_members_in_deploy_order() {
        let stack = resolve(
            &format!(
                "{HEAD}router = \"gateway\"\n\
                 [[stack.member]]\nconfig = \"gateway.toml\"\ndepends_on = [\"management\"]\n\
                 [[stack.member]]\nconfig = \"management.toml\"\n\
                 sdl = \"federation/sdl/management.graphql\"\n"
            ),
            &[
                ("gateway.toml", router("gateway")),
                ("management.toml", subgraph("management")),
            ],
        )
        .expect("valid");
        assert_eq!(stack.name, "erp");
        assert_eq!(stack.environment, "staging");
        let names: Vec<&str> = stack.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["management", "gateway"]);
        assert_eq!(
            stack.members[1].config_path,
            Path::new("/stacks/gateway.toml")
        );
        assert_eq!(stack.router.as_deref(), Some("gateway"));
        let management = &stack.members[0];
        assert_eq!(
            management.sdl.as_deref(),
            Some(Path::new("federation/sdl/management.graphql"))
        );
        assert_eq!(
            management.graphql_url(),
            Some("http://127.0.0.1:8042/graphql")
        );
    }

    /// A subgraph member: its config names the endpoint the SDL is asked from.
    fn subgraph(name: &str) -> String {
        format!(
            "{}[federation]\ngraphql_url = \"http://127.0.0.1:8042/graphql\"\n",
            member(name, "staging")
        )
    }

    #[test]
    fn a_stack_file_that_restates_the_endpoint_or_its_headers_is_refused_with_a_pointer() {
        let restated = StackFile::from_toml_str(&format!(
            "{HEAD}\n[[stack.member]]\nconfig = \"m.toml\"\nsdl = \"sdl/m.graphql\"\n\
             graphql_url = \"http://m/graphql\"\nsdl_headers = {{ \"x-api-key\" = \"M_TOKEN\" }}\n"
        ))
        .expect_err("moved keys")
        .0
        .join("\n");
        assert!(
            restated.contains("member m.toml: graphql_url moved to the member's config, as [federation].graphql_url"),
            "{restated}"
        );
        assert!(
            restated.contains(
                "member m.toml: sdl_headers is gone; the SDL query sends the member's [health] \
                 headers and token_provider"
            ),
            "{restated}"
        );
    }

    /// A router member: a release with an active path to read `sdl` from.
    fn router(name: &str) -> String {
        format!(
            "{}[artifact]\nsource = \"local\"\nactive_path = \"/srv/gateway/current\"\n",
            member(name, "staging")
        )
    }

    #[test]
    fn what_the_sdl_check_needs_and_lacks_is_reported_together() {
        let problems = resolve(
            &format!(
                "{HEAD}router = \"ghost\"\n\
                 [[stack.member]]\nconfig = \"a.toml\"\nsdl = \"sdl/a.graphql\"\n\
                 [[stack.member]]\nconfig = \"b.toml\"\n\
                 [[stack.member]]\nconfig = \"c.toml\"\nsdl = \"../c.graphql\"\n"
            ),
            &[
                ("a.toml", member("a", "staging")),
                ("b.toml", subgraph("b")),
                ("c.toml", subgraph("c")),
            ],
        )
        .expect_err("invalid");
        let all = problems.join("\n");
        assert!(all.contains("router 'ghost'"), "{all}");
        assert!(
            all.contains("member 'a' has sdl, but its config has no [federation].graphql_url"),
            "{all}"
        );
        assert!(all.contains("member 'c': sdl"), "{all}");
        assert_eq!(problems.len(), 3, "{all}");
    }

    #[test]
    fn an_sdl_needs_a_router_with_a_release_that_is_not_itself() {
        let sdl = "sdl = \"sdl/m.graphql\"\n";
        let no_router = resolve(
            &format!("{HEAD}\n[[stack.member]]\nconfig = \"m.toml\"\n{sdl}"),
            &[("m.toml", subgraph("m"))],
        )
        .expect_err("no router");
        assert!(
            no_router.iter().any(|p| p.contains("[stack].router")),
            "{no_router:?}"
        );

        let no_release = resolve(
            &format!(
                "{HEAD}router = \"gw\"\n[[stack.member]]\nconfig = \"gw.toml\"\n\
                 depends_on = [\"m\"]\n[[stack.member]]\nconfig = \"m.toml\"\n{sdl}"
            ),
            &[
                ("gw.toml", member("gw", "staging")),
                ("m.toml", subgraph("m")),
            ],
        )
        .expect_err("no active path");
        assert!(
            no_release
                .iter()
                .any(|p| p.contains("[artifact].active_path")),
            "{no_release:?}"
        );

        let on_router = resolve(
            &format!("{HEAD}router = \"gw\"\n[[stack.member]]\nconfig = \"gw.toml\"\n{sdl}"),
            &[("gw.toml", router("gw"))],
        )
        .expect_err("sdl on the router");
        assert!(
            on_router.iter().any(|p| p.contains("the router")),
            "{on_router:?}"
        );
    }

    #[test]
    fn the_router_deploys_after_every_subgraph_it_checks() {
        let problems = resolve(
            &format!(
                "{HEAD}router = \"gateway\"\n\
                 [[stack.member]]\nconfig = \"gateway.toml\"\n\
                 [[stack.member]]\nconfig = \"management.toml\"\n\
                 sdl = \"federation/sdl/management.graphql\"\n"
            ),
            &[
                ("gateway.toml", router("gateway")),
                ("management.toml", subgraph("management")),
            ],
        )
        .expect_err("the router deploys first");
        assert_eq!(
            problems,
            vec![
                "the router 'gateway' deploys before 'management', whose SDL its release \
                 holds: add 'management' to its depends_on"
                    .to_owned()
            ]
        );
    }

    #[test]
    fn a_checked_subgraph_is_not_a_blue_green_deploy() {
        let problems = resolve(
            &format!(
                "{HEAD}router = \"gateway\"\n\
                 [[stack.member]]\nconfig = \"gateway.toml\"\ndepends_on = [\"management\"]\n\
                 [[stack.member]]\nconfig = \"management.toml\"\n\
                 sdl = \"federation/sdl/management.graphql\"\n"
            ),
            &[
                ("gateway.toml", router("gateway")),
                (
                    "management.toml",
                    subgraph("management").replace(
                        "environment = \"staging\"\n",
                        "environment = \"staging\"\nstrategy = \"blue-green\"\n",
                    ),
                ),
            ],
        )
        .expect_err("blue-green");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("member 'management' has sdl, but deploys blue-green"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_stack_without_sdl_needs_no_router() {
        let stack = resolve(
            &format!("{HEAD}\n[[stack.member]]\nconfig = \"m.toml\"\n"),
            &[("m.toml", member("m", "staging"))],
        )
        .expect("valid");
        assert_eq!(stack.router, None);
    }

    #[test]
    fn every_problem_is_reported_together() {
        let problems = resolve(
            &format!(
                "{HEAD}\n[[stack.member]]\nconfig = \"a.toml\"\ndepends_on = [\"ghost\"]\n\
                 [[stack.member]]\nconfig = \"b.toml\"\n\
                 [[stack.member]]\nconfig = \"c.toml\"\n\
                 [[stack.member]]\nconfig = \"missing.toml\"\n\
                 [[stack.member]]\nconfig = \"prod.toml\"\n"
            ),
            &[
                ("a.toml", member("a", "staging")),
                ("b.toml", member("same", "staging")),
                ("c.toml", member("same", "staging")),
                ("prod.toml", member("p", "production")),
            ],
        )
        .expect_err("invalid");
        let all = problems.join("\n");
        assert!(all.contains("ghost"), "{all}");
        assert!(
            all.contains("'same'") && all.contains("b.toml") && all.contains("c.toml"),
            "{all}"
        );
        assert!(all.contains("missing.toml"), "{all}");
        assert!(all.contains("production"), "{all}");
        assert_eq!(problems.len(), 4, "{all}");
    }

    #[test]
    fn a_cycle_is_a_stack_error() {
        let problems = resolve(
            &format!(
                "{HEAD}\n[[stack.member]]\nconfig = \"a.toml\"\ndepends_on = [\"b\"]\n\
                 [[stack.member]]\nconfig = \"b.toml\"\ndepends_on = [\"a\"]\n"
            ),
            &[
                ("a.toml", member("a", "staging")),
                ("b.toml", member("b", "staging")),
            ],
        )
        .expect_err("a cycle");
        assert!(problems.iter().any(|p| p.contains("cycle")), "{problems:?}");
    }

    #[test]
    fn a_stack_needs_a_name_an_environment_and_members() {
        let problems = resolve("[stack]\n", &[]).expect_err("empty");
        let all = problems.join("\n");
        assert!(all.contains("name"), "{all}");
        assert!(all.contains("environment"), "{all}");
        assert!(all.contains("member"), "{all}");
    }

    #[test]
    fn a_member_config_without_a_name_is_an_error() {
        let problems = resolve(
            &format!("{HEAD}\n[[stack.member]]\nconfig = \"a.toml\"\n"),
            &[("a.toml", "[deploy]\nenvironment = \"staging\"\n".to_owned())],
        )
        .expect_err("no name");
        assert!(
            problems.iter().any(|p| p.contains("[deploy].name")),
            "{problems:?}"
        );
    }

    #[test]
    fn an_unknown_field_is_refused() {
        assert!(StackFile::from_toml_str(&format!(
            "{HEAD}\n[[stack.member]]\nconfig = \"a.toml\"\nkind = \"db\"\n"
        ))
        .is_err());
    }
}
