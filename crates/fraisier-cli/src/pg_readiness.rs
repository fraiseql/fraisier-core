//! The real Postgres-backed [`DatabaseReadiness`] probe behind
//! `[database.requires]`, and the bridge from that config section to it.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use fraisier_config::DeployConfig;
use fraisier_core::readiness::DatabaseReadiness;
use fraisier_db::readiness::{assess, observe, Requirements};
use fraisier_db::tviews::Emptiness;
use fraisier_db::PgConn;

/// What an empty TVIEW over a populated view costs a deploy
/// (`[database.tviews].on_empty`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnEmpty {
    /// Refuse the deploy at preflight (the default).
    Fail,
    /// Report it and continue.
    Warn,
}

/// The empty-TVIEW gate for `config`: on, failing by default, exactly when
/// `[database.requires].pg_tviews = true`. `None` means the database is never
/// asked.
pub(crate) fn tviews_gate(config: &DeployConfig) -> Option<OnEmpty> {
    let database = config.database.as_ref()?;
    if database.requires.as_ref()?.pg_tviews != Some(true) {
        return None;
    }
    let warn = database.tviews.as_ref().and_then(|t| t.on_empty.as_deref()) == Some("warn");
    Some(if warn { OnEmpty::Warn } else { OnEmpty::Fail })
}

/// What the gate makes of a probe: reasons to refuse, and warnings to print.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct GateOutcome {
    pub(crate) refusals: Vec<String>,
    pub(crate) warnings: Vec<String>,
}

/// Judge the empty-TVIEW probe's result under `on_empty`. A probe that could
/// not run has cleared nothing, so it costs what emptiness costs.
pub(crate) fn judge(on_empty: OnEmpty, probe: Result<Emptiness, String>) -> GateOutcome {
    let findings = match probe {
        Err(error) => vec![format!("could not check TVIEWs for emptiness: {error}")],
        Ok(found) => {
            let mut findings = Vec::new();
            if !found.empty.is_empty() {
                let pairs: Vec<String> = found
                    .empty
                    .iter()
                    .map(|t| format!("{} (view {})", t.qualified(), t.view()))
                    .collect();
                findings.push(format!(
                    "{} pg_tviews TVIEW(s) are empty while their backing view has rows: {}. \
                     A restore, a crash-recovery start or a failover can leave a TVIEW \
                     empty, and nothing else notices. Run `fraisier db tviews rebuild` \
                     (add --all for a LOGGED TVIEW or one emptied by hand)",
                    found.empty.len(),
                    pairs.join(", ")
                ));
            }
            if !found.unreadable.is_empty() {
                let named: Vec<String> = found
                    .unreadable
                    .iter()
                    .map(|(t, grant)| format!("{} (view {}): {grant}", t.qualified(), t.view()))
                    .collect();
                findings.push(format!(
                    "{} pg_tviews TVIEW(s) cannot be read by this role, so they were not \
                     checked for emptiness: {}",
                    found.unreadable.len(),
                    named.join("; ")
                ));
            }
            findings
        }
    };
    match on_empty {
        OnEmpty::Fail => GateOutcome {
            refusals: findings,
            warnings: Vec::new(),
        },
        OnEmpty::Warn => GateOutcome {
            refusals: Vec::new(),
            warnings: findings,
        },
    }
}

/// Asks the live server, through `psql`, for what [`assess`] checks.
pub(crate) struct PgReadiness {
    requirements: Requirements,
    tviews_gate: Option<OnEmpty>,
    psql: PathBuf,
}

/// The requirements `[database.requires]` declares, or `None` when it declares
/// nothing (no section, or no requirement in it).
pub(crate) fn requirements(config: &DeployConfig) -> Option<Requirements> {
    let requires = config.database.as_ref()?.requires.as_ref()?;
    let requirements = Requirements {
        postgres_major: requires.postgres_major,
        pg_tviews: requires.pg_tviews == Some(true),
    };
    (requirements != Requirements::default()).then_some(requirements)
}

/// The readiness probe for `config`, when it declares any requirement.
pub(crate) fn probe(config: &DeployConfig) -> Option<Arc<dyn DatabaseReadiness>> {
    requirements(config).map(|requirements| {
        Arc::new(PgReadiness {
            requirements,
            tviews_gate: tviews_gate(config),
            psql: PathBuf::from("psql"),
        }) as Arc<dyn DatabaseReadiness>
    })
}

#[async_trait]
impl DatabaseReadiness for PgReadiness {
    async fn unready(&self, live_dsn: &str) -> Result<Vec<String>, String> {
        let conn = PgConn::parse(live_dsn).map_err(|error| error.to_string())?;
        let observed = observe(&self.psql, &conn).await?;
        let unready: Vec<String> = assess(&self.requirements, &observed)
            .iter()
            .map(ToString::to_string)
            .collect();
        // The gate runs right after readiness, and only on a ready server: a
        // missing or outdated pg_tviews is already the refusal.
        let (Some(on_empty), true) = (self.tviews_gate, unready.is_empty()) else {
            return Ok(unready);
        };
        let probe = match observed.tviews_schema.as_deref() {
            Some(schema) => fraisier_db::tviews::find_empty(&self.psql, &conn, schema).await,
            None => Err("pg_tviews is not installed in this database".to_owned()),
        };
        let outcome = judge(on_empty, probe);
        for warning in &outcome.warnings {
            eprintln!("warning: {}", fraisier_saga::redact::credentials(warning));
        }
        Ok(outcome.refusals)
    }
}

// The Python implementation warned, rather than refused, when the probe could
// not run under the default gate. That case is deliberately not carried over:
// D2 makes `fail` the default, and a probe that could not run has cleared
// nothing, so it costs what emptiness costs (see `judge`).
#[cfg(test)]
mod tests {
    use fraisier_config::DeployConfig;
    use fraisier_db::readiness::Requirements;

    use fraisier_db::tviews::{Emptiness, Tview};

    use super::{judge, requirements, tviews_gate, GateOutcome, OnEmpty};

    fn tview(name: &str) -> Tview {
        Tview {
            schema: "public".to_owned(),
            name: name.to_owned(),
            view_schema: "tviews".to_owned(),
            view_name: format!("public__{name}"),
            readable: true,
        }
    }

    fn empty(names: &[&str]) -> Emptiness {
        Emptiness {
            empty: names.iter().map(|n| tview(n)).collect(),
            unreadable: Vec::new(),
        }
    }

    #[test]
    fn the_gate_is_declared_by_pg_tviews_and_fails_by_default() {
        let config =
            DeployConfig::from_toml_str(&format!("{BASE}[database.requires]\npg_tviews = true\n"))
                .expect("parses");
        assert_eq!(tviews_gate(&config), Some(OnEmpty::Fail));
        let warn = DeployConfig::from_toml_str(&format!(
            "{BASE}[database.requires]\npg_tviews = true\n\n[database.tviews]\non_empty = \"warn\"\n"
        ))
        .expect("parses");
        assert_eq!(tviews_gate(&warn), Some(OnEmpty::Warn));
    }

    #[test]
    fn a_database_that_does_not_declare_pg_tviews_is_never_asked() {
        for section in ["", "[database.requires]\npostgres_major = 18\n"] {
            let config = DeployConfig::from_toml_str(&format!("{BASE}{section}")).expect("parses");
            assert_eq!(tviews_gate(&config), None, "{section:?}");
        }
    }

    #[test]
    fn an_empty_tview_over_rows_refuses_naming_each_pair_and_the_command() {
        let outcome = judge(OnEmpty::Fail, Ok(empty(&["tv_a", "tv_b"])));
        assert_eq!(outcome.warnings, Vec::<String>::new());
        let [refusal] = &outcome.refusals[..] else {
            panic!("{outcome:?}")
        };
        assert!(
            refusal.contains("public.tv_a (view tviews.public__tv_a)"),
            "{refusal}"
        );
        assert!(
            refusal.contains("public.tv_b (view tviews.public__tv_b)"),
            "{refusal}"
        );
        assert!(refusal.contains("fraisier db tviews rebuild"), "{refusal}");
    }

    #[test]
    fn on_empty_warn_warns_and_continues() {
        let outcome = judge(OnEmpty::Warn, Ok(empty(&["tv_a"])));
        assert_eq!(outcome.refusals, Vec::<String>::new());
        assert!(outcome.warnings[0].contains("public.tv_a"), "{outcome:?}");
    }

    #[test]
    fn a_probe_that_cannot_run_fails_and_with_warn_warns() {
        let failed = judge(OnEmpty::Fail, Err("connection refused".to_owned()));
        assert!(
            failed.refusals[0].contains("could not check TVIEWs for emptiness: connection refused"),
            "{failed:?}"
        );
        let warned = judge(OnEmpty::Warn, Err("connection refused".to_owned()));
        assert_eq!(warned.refusals, Vec::<String>::new());
        assert!(
            warned.warnings[0].contains("connection refused"),
            "{warned:?}"
        );
    }

    #[test]
    fn nothing_empty_passes() {
        assert_eq!(
            judge(OnEmpty::Fail, Ok(Emptiness::default())),
            GateOutcome::default()
        );
    }

    #[test]
    fn an_unreadable_tview_fails_with_its_grant() {
        let probe = Emptiness {
            empty: Vec::new(),
            unreadable: vec![(
                tview("tv_a"),
                "GRANT SELECT ON \"public\".\"tv_a\" TO \"app\"".to_owned(),
            )],
        };
        let outcome = judge(OnEmpty::Fail, Ok(probe.clone()));
        assert!(
            outcome.refusals[0].contains("GRANT SELECT ON \"public\".\"tv_a\" TO \"app\""),
            "{outcome:?}"
        );
        assert_eq!(
            judge(OnEmpty::Warn, Ok(probe)).refusals,
            Vec::<String>::new()
        );
    }

    const BASE: &str = "[deploy]\nname = \"app\"\nenvironment = \"prod\"\n";

    #[test]
    fn no_section_means_no_probe() {
        let config = DeployConfig::from_toml_str(BASE).expect("parses");
        assert_eq!(requirements(&config), None);
    }

    #[test]
    fn pg_tviews_false_alone_declares_nothing() {
        let config =
            DeployConfig::from_toml_str(&format!("{BASE}[database.requires]\npg_tviews = false\n"))
                .expect("parses");
        assert_eq!(requirements(&config), None);
    }

    #[test]
    fn the_section_maps_onto_the_probes_requirements() {
        let config = DeployConfig::from_toml_str(&format!(
            "{BASE}[database.requires]\npostgres_major = 18\npg_tviews = true\n"
        ))
        .expect("parses");
        assert_eq!(
            requirements(&config),
            Some(Requirements {
                postgres_major: Some(18),
                pg_tviews: true
            })
        );
    }
}
