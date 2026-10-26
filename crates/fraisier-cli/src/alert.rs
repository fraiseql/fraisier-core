//! Failure alerting for the scheduled backup, prune and drill units.
//!
//! The failing run only *records* ([`record_failure`]); the unit's
//! `OnFailure=` alert unit runs `fraisier notify` ([`notify`]), which sends the
//! record through `[notify].command`. A scheduled deploy is not on this path:
//! it notifies in-process when it rolls back, and its unit has no `OnFailure=`,
//! so no failure is reported twice.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::json;

use crate::commands::CommandOutput;
use crate::failure_record::{self, FailureRecord};

/// The scheduled runs that report through a failure record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduledRun {
    /// `fraisier backup`.
    Backup,
    /// `fraisier backup prune`.
    Prune,
    /// `fraisier backup drill`.
    Drill,
}

impl ScheduledRun {
    /// The payload event a failure of this run is reported as.
    pub(crate) const fn event(self) -> &'static str {
        match self {
            Self::Backup => "scheduled-backup-failed",
            Self::Prune => "scheduled-prune-failed",
            Self::Drill => "scheduled-drill-failed",
        }
    }
}

/// After a run started with `--failure-record <unit>`: when `outcome` is a
/// failure, write its record into `dir` for the alert to send. Returns the
/// record's path, `None` for a run that succeeded, or a warning to print when
/// no record could be written (the alert then still fires, with a journal
/// pointer instead of the reason).
pub(crate) fn record_failure(
    config_path: &Path,
    run: ScheduledRun,
    unit: &str,
    outcome: &Result<CommandOutput>,
    invocation_id: Option<&str>,
    dir: &Path,
) -> Result<Option<PathBuf>, String> {
    let reason = match outcome {
        Ok(output) if output.exit_code == 0 => return Ok(None),
        Ok(output) => output.pretty.trim().to_owned(),
        Err(error) => crate::commands::error_detail(error),
    };
    let Some(invocation_id) = invocation_id else {
        return Err(
            "no failure record written: INVOCATION_ID is not set (not run by systemd?)".to_owned(),
        );
    };
    let deploy = crate::commands::load(config_path)
        .ok()
        .and_then(|config| config.deploy);
    let record = FailureRecord {
        event: run.event().to_owned(),
        fraise: deploy.as_ref().and_then(|d| d.name.clone()),
        environment: deploy.and_then(|d| d.environment),
        unit: unit.to_owned(),
        invocation_id: invocation_id.to_owned(),
        at_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        reason,
    };
    failure_record::write(dir, &record)
        .map(Some)
        .map_err(|error| format!("no failure record written to {}: {error}", dir.display()))
}

/// The event for a failed unit that left no record, read off the unit names
/// `scheduled install` generates.
fn event_for_unit(unit: &str) -> &'static str {
    let name = unit.strip_suffix(".service").unwrap_or(unit);
    if name.ends_with("-drill") {
        ScheduledRun::Drill.event()
    } else if name.contains("-retain-") {
        ScheduledRun::Prune.event()
    } else if name.ends_with("-scheduled") {
        ScheduledRun::Backup.event()
    } else {
        "scheduled-unit-failed"
    }
}

/// `fraisier notify --unit <unit>`: the body of the `OnFailure=` alert unit.
///
/// It sends the failed activation's record, found by `monitor_invocation_id`
/// (systemd's `$MONITOR_INVOCATION_ID`), through `[notify].command` once, and
/// deletes the record. Without a matching record it still notifies, pointing at
/// the journal. Without a sink it writes the payload to stderr, which the
/// journal keeps.
///
/// # Errors
/// When the config cannot be read.
pub(crate) async fn notify(
    config_path: &Path,
    unit: &str,
    monitor_invocation_id: Option<&str>,
    dir: &Path,
) -> Result<CommandOutput> {
    use fraisier_self_upgrade::Notifier as _;

    let config = crate::commands::load(config_path)?;
    let invocation_id = monitor_invocation_id.filter(|id| failure_record::is_invocation_id(id));
    let found = invocation_id.and_then(|id| failure_record::read(dir, id));
    let record = found.as_ref().map(|(record, _)| record).filter(|record| {
        Some(record.invocation_id.as_str()) == invocation_id && record.unit == unit
    });

    let deploy = config.deploy.as_ref();
    let fraise = record
        .and_then(|r| r.fraise.clone())
        .or_else(|| deploy.and_then(|d| d.name.clone()));
    let environment = record
        .and_then(|r| r.environment.clone())
        .or_else(|| deploy.and_then(|d| d.environment.clone()));
    let pointer = invocation_id.map_or_else(
        || format!("-u {unit}"),
        |id| format!("_SYSTEMD_INVOCATION_ID={id}"),
    );
    let payload = fraisier_self_upgrade::FailurePayload {
        event: record.map_or_else(|| event_for_unit(unit).to_owned(), |r| r.event.clone()),
        failed: match (&fraise, &environment) {
            (Some(fraise), Some(environment)) => Some(format!("{fraise}/{environment}")),
            _ => None,
        },
        restored: None,
        // Redacted again: the record was redacted when written, but whatever
        // else can write to the directory was not.
        reason: record.map_or_else(
            || format!("no failure record; see journalctl {pointer}"),
            |r| fraisier_saga::redact::credentials(&r.reason),
        ),
    };

    let sink = config.notify.as_ref().and_then(|n| n.command.as_deref());
    let delivered = if let Some(command) = sink {
        let mut notifier = fraisier_self_upgrade::ExecHookNotifier::new(command)
            .with_context("FRAISIER_NOTIFY_UNIT", unit);
        for (key, value) in [
            ("FRAISIER_NOTIFY_FRAISE", fraise.as_deref()),
            ("FRAISIER_NOTIFY_ENVIRONMENT", environment.as_deref()),
            ("FRAISIER_NOTIFY_INVOCATION_ID", invocation_id),
        ] {
            if let Some(value) = value {
                notifier = notifier.with_context(key, value);
            }
        }
        notifier.notify(&payload).await;
        format!("sent {} for {unit} through [notify].command", payload.event)
    } else {
        let line = format!(
            "no [notify].command; {} for {unit}: {}",
            payload.event,
            serde_json::to_string(&payload).unwrap_or_default()
        );
        eprintln!("{line}");
        line
    };
    // One attempt, then the record is gone, matched or not: a record left
    // behind could only be sent again.
    if let Some((_, path)) = &found {
        let _ = std::fs::remove_file(path);
    }

    Ok(CommandOutput {
        exit_code: 0,
        pretty: format!("{delivered}\n"),
        json: json!({
            "event": payload.event,
            "unit": unit,
            "record": record.is_some(),
            "sink": sink.is_some(),
        }),
    })
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::path::{Path, PathBuf};

    use super::{notify, record_failure, ScheduledRun};
    use crate::commands::CommandOutput;
    use crate::failure_record::{self, FailureRecord};

    const ID: &str = "0123456789abcdef0123456789abcdef";
    const UNIT: &str = "fraisier-checkout-production-scheduled.service";

    struct Fixture {
        _tmp: tempfile::TempDir,
        config: PathBuf,
        records: PathBuf,
        hook_log: PathBuf,
    }

    /// A config whose `[notify].command` appends one `event|reason|unit` line per
    /// call, so a test can count deliveries.
    fn fixture(with_sink: bool) -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hook_log = tmp.path().join("hook.log");
        let mut toml =
            String::from("[deploy]\nname = \"checkout\"\nenvironment = \"production\"\n");
        if with_sink {
            let _ = write!(
                toml,
                "\n[notify]\ncommand = 'printf \"%s|%s|%s|%s\\n\" \"$FRAISIER_NOTIFY_EVENT\" \
                 \"$FRAISIER_NOTIFY_REASON\" \"$FRAISIER_NOTIFY_UNIT\" \
                 \"$FRAISIER_NOTIFY_FAILED\" >> {}'\n",
                hook_log.display()
            );
        }
        let config = tmp.path().join("fraisier.toml");
        std::fs::write(&config, toml).expect("config");
        Fixture {
            records: tmp.path().join("failures"),
            config,
            hook_log,
            _tmp: tmp,
        }
    }

    fn deliveries(fixture: &Fixture) -> Vec<String> {
        std::fs::read_to_string(&fixture.hook_log)
            .unwrap_or_default()
            .lines()
            .map(ToOwned::to_owned)
            .collect()
    }

    fn failed(pretty: &str) -> CommandOutput {
        CommandOutput {
            exit_code: 1,
            pretty: pretty.to_owned(),
            json: serde_json::json!({}),
        }
    }

    fn stored(fixture: &Fixture, record: &FailureRecord) -> PathBuf {
        failure_record::write(&fixture.records, record).expect("write record")
    }

    fn a_record(reason: &str) -> FailureRecord {
        FailureRecord {
            event: "scheduled-drill-failed".to_owned(),
            fraise: Some("checkout".to_owned()),
            environment: Some("production".to_owned()),
            unit: UNIT.to_owned(),
            invocation_id: ID.to_owned(),
            at_unix: 1_790_000_000,
            reason: reason.to_owned(),
        }
    }

    #[test]
    fn a_failed_run_writes_its_record() {
        let fixture = fixture(true);
        let path = record_failure(
            &fixture.config,
            ScheduledRun::Backup,
            UNIT,
            &Ok(failed("backup failed: pg_dump exited 1\n")),
            Some(ID),
            &fixture.records,
        )
        .expect("recorded")
        .expect("a failure leaves a record");
        let (record, _) = failure_record::read(&fixture.records, ID).expect("read");
        assert_eq!(path, fixture.records.join(format!("{ID}.json")));
        assert_eq!(record.event, "scheduled-backup-failed");
        assert_eq!(record.fraise.as_deref(), Some("checkout"));
        assert_eq!(record.environment.as_deref(), Some("production"));
        assert_eq!(record.unit, UNIT);
        assert_eq!(record.reason, "backup failed: pg_dump exited 1");
    }

    #[test]
    fn an_error_is_recorded_with_its_chain() {
        let fixture = fixture(true);
        let outcome = Err(anyhow::anyhow!("exit 1").context("pg_dump failed"));
        record_failure(
            &fixture.config,
            ScheduledRun::Prune,
            UNIT,
            &outcome,
            Some(ID),
            &fixture.records,
        )
        .expect("recorded");
        let (record, _) = failure_record::read(&fixture.records, ID).expect("read");
        assert_eq!(record.event, "scheduled-prune-failed");
        assert!(
            record.reason.contains("pg_dump failed"),
            "{}",
            record.reason
        );
        assert!(record.reason.contains("exit 1"), "{}", record.reason);
    }

    #[test]
    fn a_successful_run_writes_nothing() {
        let fixture = fixture(true);
        let ok = Ok(CommandOutput {
            exit_code: 0,
            pretty: String::new(),
            json: serde_json::json!({}),
        });
        let recorded = record_failure(
            &fixture.config,
            ScheduledRun::Drill,
            UNIT,
            &ok,
            Some(ID),
            &fixture.records,
        );
        assert_eq!(recorded, Ok(None));
        assert!(!fixture.records.exists());
    }

    #[test]
    fn without_an_invocation_id_nothing_is_written_and_it_says_so() {
        let fixture = fixture(true);
        let recorded = record_failure(
            &fixture.config,
            ScheduledRun::Backup,
            UNIT,
            &Ok(failed("boom")),
            None,
            &fixture.records,
        );
        let warning = recorded.expect_err("a warning");
        assert!(warning.contains("INVOCATION_ID"), "{warning}");
        assert!(!fixture.records.exists());
    }

    #[tokio::test]
    async fn a_failing_backup_records_and_does_not_notify_in_process() {
        // The backup fails (no database configured); recording it sends nothing.
        let fixture = fixture(true);
        let outcome = crate::commands::db_backup(&fixture.config, None, false).await;
        assert!(
            outcome.as_ref().map_or(true, |out| out.exit_code != 0),
            "the backup must fail for this test"
        );
        record_failure(
            &fixture.config,
            ScheduledRun::Backup,
            UNIT,
            &outcome,
            Some(ID),
            &fixture.records,
        )
        .expect("recorded");
        assert!(failure_record::read(&fixture.records, ID).is_some());
        assert_eq!(deliveries(&fixture), Vec::<String>::new());
    }

    #[tokio::test]
    async fn notify_sends_the_record_once_and_deletes_it() {
        let fixture = fixture(true);
        let path = stored(&fixture, &a_record("drill failed at restore"));
        let out = notify(&fixture.config, UNIT, Some(ID), &fixture.records)
            .await
            .expect("notify");
        assert_eq!(out.exit_code, 0);
        assert_eq!(
            deliveries(&fixture),
            vec![format!(
                "scheduled-drill-failed|drill failed at restore|{UNIT}|checkout/production"
            )]
        );
        assert!(!path.exists(), "the record is deleted once sent");
    }

    #[tokio::test]
    async fn notify_redacts_a_record_again_on_read() {
        // The disk is a sink too: a record written by anything else is redacted.
        let fixture = fixture(true);
        std::fs::create_dir_all(&fixture.records).expect("dir");
        let raw = serde_json::to_string(&a_record("connect postgres://app:s3cr3t-pw@db/shop"))
            .expect("json");
        std::fs::write(fixture.records.join(format!("{ID}.json")), raw).expect("write");
        notify(&fixture.config, UNIT, Some(ID), &fixture.records)
            .await
            .expect("notify");
        let sent = deliveries(&fixture).join("\n");
        assert!(!sent.contains("s3cr3t-pw"), "{sent}");
        assert!(sent.contains("postgres://"), "{sent}");
    }

    #[tokio::test]
    async fn without_a_record_notify_points_at_the_invocation() {
        let fixture = fixture(true);
        notify(&fixture.config, UNIT, Some(ID), &fixture.records)
            .await
            .expect("notify");
        assert_eq!(
            deliveries(&fixture),
            vec![format!(
                "scheduled-backup-failed|no failure record; see journalctl \
                 _SYSTEMD_INVOCATION_ID={ID}|{UNIT}|checkout/production"
            )]
        );
    }

    #[tokio::test]
    async fn without_an_invocation_id_notify_points_at_the_unit() {
        let fixture = fixture(true);
        notify(&fixture.config, UNIT, None, &fixture.records)
            .await
            .expect("notify");
        assert_eq!(
            deliveries(&fixture),
            vec![format!(
                "scheduled-backup-failed|no failure record; see journalctl -u {UNIT}|{UNIT}|\
                 checkout/production"
            )]
        );
    }

    #[tokio::test]
    async fn a_record_for_another_unit_is_not_sent() {
        let fixture = fixture(true);
        let mut other = a_record("someone else's failure");
        other.unit = "fraisier-checkout-production-drill.service".to_owned();
        let path = stored(&fixture, &other);
        notify(&fixture.config, UNIT, Some(ID), &fixture.records)
            .await
            .expect("notify");
        let sent = deliveries(&fixture);
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0].contains("no failure record"), "{sent:?}");
        assert!(
            !path.exists(),
            "a mismatched record is not left to be sent later"
        );
    }

    #[tokio::test]
    async fn the_event_without_a_record_follows_the_unit_name() {
        for (unit, event) in [
            (
                "fraisier-checkout-production-drill.service",
                "scheduled-drill-failed",
            ),
            (
                "fraisier-checkout-production-retain-nightly.service",
                "scheduled-prune-failed",
            ),
            (
                "fraisier-checkout-production-scheduled.service",
                "scheduled-backup-failed",
            ),
            ("something-else.service", "scheduled-unit-failed"),
        ] {
            let fixture = fixture(true);
            notify(&fixture.config, unit, None, &fixture.records)
                .await
                .expect("notify");
            let sent = deliveries(&fixture);
            assert!(
                sent[0].starts_with(&format!("{event}|")),
                "{unit}: {sent:?}"
            );
        }
    }

    #[tokio::test]
    async fn without_a_sink_notify_still_succeeds_and_consumes_the_record() {
        let fixture = fixture(false);
        let path = stored(&fixture, &a_record("drill failed"));
        let out = notify(&fixture.config, UNIT, Some(ID), &fixture.records)
            .await
            .expect("notify");
        assert_eq!(out.exit_code, 0);
        assert!(!path.exists());
        assert!(out.pretty.contains("no [notify].command"), "{}", out.pretty);
    }

    #[test]
    fn the_record_directory_helper_is_the_one_notify_reads() {
        // Writer and reader agree on the path for the same environment.
        assert_eq!(
            failure_record::dir(None),
            Path::new("/run/fraisier/failures").to_path_buf()
        );
    }
}
