//! The scheduled backup, prune and drill units, and the alert unit their
//! `OnFailure=` names. Golden files: each test pins one unit byte for byte.

use fraisier_config::DeployConfig;
use fraisier_scaffold::{generate, generate_scheduled, GeneratedFile, MARKER};

const BASE: &str = r#"
[deploy]
name = "checkout"
environment = "production"
"#;

const ALERT: &str = "fraisier-checkout-production-alert@.service";

fn units(extra: &str) -> Vec<GeneratedFile> {
    let config = DeployConfig::from_toml_str(&format!("{BASE}{extra}")).expect("parses");
    generate_scheduled(&config).expect("generates")
}

fn unit<'a>(files: &'a [GeneratedFile], name: &str) -> &'a GeneratedFile {
    files
        .iter()
        .find(|file| file.rel_path.file_name().is_some_and(|n| n == name))
        .unwrap_or_else(|| {
            let names: Vec<_> = files
                .iter()
                .map(|f| f.rel_path.display().to_string())
                .collect();
            panic!("no {name} in {names:?}")
        })
}

/// Whether `file` sets `directive` (a line, not a mention in a comment).
fn sets(file: &GeneratedFile, directive: &str) -> bool {
    file.contents
        .lines()
        .any(|line| line.starts_with(directive))
}

fn names(files: &[GeneratedFile]) -> Vec<String> {
    let mut names: Vec<String> = files
        .iter()
        .map(|f| {
            f.rel_path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

// The scheduled backup service's own golden lives with the generator
// (`a_scheduled_backup_service_is_exactly_this_unit`).
const BACKUP: &str = "\n[backup]\ndir = \"/var/backups/checkout\"\n\n\
                      [schedule]\ncalendar = \"daily 03:00\"\ncommand = \"backup\"\n";

const RETAIN: &str = "\n[[backup.retain]]\ndir = \"/var/backups/checkout\"\nname = \"nightly\"\n\
                      schedule = \"daily 05:30\"\nretention_days = 14\n";

const DRILL: &str = "\n[backup]\ndir = \"/var/backups/checkout\"\n\n\
                     [drill]\nschedule = \"weekly Sun 04:00\"\n";

#[test]
fn the_alert_unit_is_exactly_this_template() {
    let files = units(BACKUP);
    let alert = unit(&files, ALERT);
    assert_eq!(
        alert.contents,
        format!(
            "# {MARKER}\n\
             [Unit]\n\
             Description=fraisier failure alert for checkout (production): %i\n\
             \n\
             [Service]\n\
             Type=oneshot\n\
             # Sends the failed unit's record through [notify].command, once. This\n\
             # unit has no OnFailure=: a failed alert is logged, never re-alerted.\n\
             ExecStart=fraisier notify --config /etc/fraisier/checkout.toml --unit %i\n\
             EnvironmentFile=-/etc/checkout/production.env\n\
             # Operator-only secrets such as [database].admin_url_env; the app\n\
             # unit never loads this file.\n\
             EnvironmentFile=-/etc/checkout/production.ops.env\n"
        )
    );
    assert_eq!(
        alert.install_dest.as_deref(),
        Some(std::path::Path::new(
            "/etc/systemd/system/fraisier-checkout-production-alert@.service"
        ))
    );
}

#[test]
fn a_retain_entry_gets_a_prune_service_and_timer() {
    let files = units(RETAIN);
    assert_eq!(
        unit(&files, "fraisier-checkout-production-retain-nightly.service").contents,
        format!(
            "# {MARKER}\n\
             [Unit]\n\
             Description=fraisier prune of nightly archives for checkout (production)\n\
             OnFailure=fraisier-checkout-production-alert@%n.service\n\
             \n\
             [Service]\n\
             Type=oneshot\n\
             # The policy (dir, match, retention, floor) stays in the config: this\n\
             # unit only names the [[backup.retain]] entry.\n\
             ExecStart=fraisier backup prune --config /etc/fraisier/checkout.toml --name nightly --apply --failure-record %n\n\
             EnvironmentFile=-/etc/checkout/production.env\n\
             # Operator-only secrets such as [database].admin_url_env; the app\n\
             # unit never loads this file.\n\
             EnvironmentFile=-/etc/checkout/production.ops.env\n\
             # A failed run leaves its redacted reason here for the alert unit.\n\
             RuntimeDirectory=fraisier/failures\n\
             RuntimeDirectoryPreserve=yes\n"
        )
    );
    assert_eq!(
        unit(&files, "fraisier-checkout-production-retain-nightly.timer").contents,
        format!(
            "# {MARKER}\n\
             [Unit]\n\
             Description=fraisier prune timer of nightly archives for checkout (production)\n\
             \n\
             [Timer]\n\
             # Exactly one OnCalendar=: systemd adds every non-empty assignment as\n\
             # another trigger rather than replacing the last.\n\
             OnCalendar=*-*-* 05:30:00\n\
             # A host that was down at the hour prunes on boot.\n\
             Persistent=true\n\
             RandomizedDelaySec=300\n\
             \n\
             [Install]\n\
             WantedBy=timers.target\n"
        )
    );
}

#[test]
fn a_scheduled_drill_gets_a_service_and_timer() {
    let files = units(DRILL);
    assert_eq!(
        unit(&files, "fraisier-checkout-production-drill.service").contents,
        format!(
            "# {MARKER}\n\
             [Unit]\n\
             Description=fraisier restore drill for checkout (production)\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             OnFailure=fraisier-checkout-production-alert@%n.service\n\
             \n\
             [Service]\n\
             Type=oneshot\n\
             ExecStart=fraisier backup drill --config /etc/fraisier/checkout.toml --failure-record %n\n\
             EnvironmentFile=-/etc/checkout/production.env\n\
             # Operator-only secrets such as [database].admin_url_env; the app\n\
             # unit never loads this file.\n\
             EnvironmentFile=-/etc/checkout/production.ops.env\n\
             # A failed run leaves its redacted reason here for the alert unit.\n\
             RuntimeDirectory=fraisier/failures\n\
             RuntimeDirectoryPreserve=yes\n"
        )
    );
    assert_eq!(
        unit(&files, "fraisier-checkout-production-drill.timer").contents,
        format!(
            "# {MARKER}\n\
             [Unit]\n\
             Description=fraisier restore drill timer for checkout (production)\n\
             \n\
             [Timer]\n\
             # Exactly one OnCalendar=: systemd adds every non-empty assignment as\n\
             # another trigger rather than replacing the last.\n\
             OnCalendar=Sun *-*-* 04:00:00\n\
             Persistent=true\n\
             \n\
             [Install]\n\
             WantedBy=timers.target\n"
        )
    );
}

#[test]
fn a_prune_only_host_gets_its_units_and_the_alert_without_a_schedule() {
    // No [schedule]: the alert path must not depend on one.
    assert_eq!(
        names(&units(RETAIN)),
        vec![
            ALERT.to_owned(),
            "fraisier-checkout-production-retain-nightly.service".to_owned(),
            "fraisier-checkout-production-retain-nightly.timer".to_owned(),
        ]
    );
}

#[test]
fn every_referencing_unit_comes_with_the_alert_unit() {
    let all = units(&format!(
        "{BACKUP}\n[drill]\nschedule = \"weekly Sun 04:00\"\n{RETAIN}"
    ));
    assert_eq!(
        names(&all),
        vec![
            ALERT.to_owned(),
            "fraisier-checkout-production-drill.service".to_owned(),
            "fraisier-checkout-production-drill.timer".to_owned(),
            "fraisier-checkout-production-retain-nightly.service".to_owned(),
            "fraisier-checkout-production-retain-nightly.timer".to_owned(),
            "fraisier-checkout-production-scheduled.service".to_owned(),
            "fraisier-checkout-production-scheduled.timer".to_owned(),
        ]
    );
    for file in &all {
        let references = sets(file, "OnFailure=");
        let name = file.rel_path.display().to_string();
        let extension = file.rel_path.extension().and_then(|e| e.to_str());
        if extension == Some("service") && !name.ends_with(ALERT) {
            assert!(references, "{name} must alert");
        }
        if extension == Some("timer") {
            assert_eq!(file.contents.matches("\nOnCalendar=").count(), 1, "{name}");
        }
    }
    assert!(!sets(unit(&all, ALERT), "OnFailure="));
}

#[test]
fn a_scheduled_deploy_notifies_in_process_and_gets_no_alert() {
    let files = units(
        "\n[schedule]\ncalendar = \"daily 03:00\"\ncommand = \"deploy\"\n\
         allow_unattended_deploy = true\n\n[notify]\ncommand = \"true\"\n",
    );
    assert_eq!(
        names(&files),
        vec![
            "fraisier-checkout-production-scheduled.service".to_owned(),
            "fraisier-checkout-production-scheduled.timer".to_owned(),
        ]
    );
    let service = unit(&files, "fraisier-checkout-production-scheduled.service");
    assert!(
        !service.contents.contains("OnFailure="),
        "{}",
        service.contents
    );
    assert!(
        !service.contents.contains("--failure-record"),
        "{}",
        service.contents
    );
    assert!(
        service
            .contents
            .contains("ExecStart=fraisier deploy --config /etc/fraisier/checkout.toml\n"),
        "{}",
        service.contents
    );
}

#[test]
fn an_unscheduled_drill_gets_no_unit() {
    let files = units(&format!("{RETAIN}\n[drill]\n"));
    assert!(
        !names(&files).iter().any(|n| n.contains("drill")),
        "{:?}",
        names(&files)
    );
}

#[test]
fn nothing_to_schedule_is_an_error() {
    let config = DeployConfig::from_toml_str(BASE).expect("parses");
    assert!(generate_scheduled(&config).is_err());
}

#[test]
fn a_custom_config_path_reaches_every_unit() {
    let all = units(&format!(
        "{BACKUP}config_path = \"/srv/checkout/fraisier.toml\"\n{RETAIN}"
    ));
    for name in [
        ALERT,
        "fraisier-checkout-production-scheduled.service",
        "fraisier-checkout-production-retain-nightly.service",
    ] {
        assert!(
            unit(&all, name)
                .contents
                .contains("--config /srv/checkout/fraisier.toml "),
            "{name}"
        );
    }
}

/// The `EnvironmentFile=` lines of `file`, in order.
fn env_files(file: &GeneratedFile) -> Vec<&str> {
    file.contents
        .lines()
        .filter_map(|line| line.strip_prefix("EnvironmentFile=-"))
        .collect()
}

#[test]
fn every_scheduled_unit_loads_the_ops_env_file_after_the_shared_one() {
    // The ops file comes second so a value in it wins over the shared one.
    let all = units(&format!(
        "{BACKUP}\n[drill]\nschedule = \"weekly Sun 04:00\"\n{RETAIN}"
    ));
    for file in all
        .iter()
        .filter(|f| f.rel_path.extension().is_some_and(|e| e == "service"))
    {
        assert_eq!(
            env_files(file),
            vec![
                "/etc/checkout/production.env",
                "/etc/checkout/production.ops.env"
            ],
            "{}",
            file.rel_path.display()
        );
    }
}

#[test]
fn the_app_unit_never_loads_the_ops_env_file() {
    // An admin DSN set for the drill must not reach the application.
    let config = DeployConfig::from_toml_str(BASE).expect("parses");
    let files = generate(&config).expect("generates");
    let app = unit(&files, "checkout.service");
    assert_eq!(env_files(app), vec!["/etc/checkout/production.env"]);
}
