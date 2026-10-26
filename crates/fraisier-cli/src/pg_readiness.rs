//! The real Postgres-backed [`DatabaseReadiness`] probe behind
//! `[database.requires]`, and the bridge from that config section to it.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use fraisier_config::DeployConfig;
use fraisier_core::readiness::DatabaseReadiness;
use fraisier_db::readiness::{assess, observe, Requirements};
use fraisier_db::PgConn;

/// Asks the live server, through `psql`, for what [`assess`] checks.
pub(crate) struct PgReadiness {
    requirements: Requirements,
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
            psql: PathBuf::from("psql"),
        }) as Arc<dyn DatabaseReadiness>
    })
}

#[async_trait]
impl DatabaseReadiness for PgReadiness {
    async fn unready(&self, live_dsn: &str) -> Result<Vec<String>, String> {
        let conn = PgConn::parse(live_dsn).map_err(|error| error.to_string())?;
        let observed = observe(&self.psql, &conn).await?;
        Ok(assess(&self.requirements, &observed)
            .iter()
            .map(ToString::to_string)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use fraisier_config::DeployConfig;
    use fraisier_db::readiness::Requirements;

    use super::requirements;

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
