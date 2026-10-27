//! Prometheus textfile metrics: each command writes its own `.prom` file into
//! `[metrics].textfile_dir`, for `node_exporter`'s textfile collector to serve.
//!
//! A one-shot CLI has no `up` and keeps no counter across runs, so every metric is
//! a gauge describing the last run. Labels are only `fraise` and `environment`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The `fraise` and `environment` labels every sample carries.
pub(crate) struct Labels<'a> {
    pub fraise: &'a str,
    pub environment: &'a str,
}

/// A gauge's value, rendered exactly.
#[derive(Debug, Clone, Copy)]
enum Value {
    Count(u64),
    Seconds(f64),
}

struct Gauge {
    name: &'static str,
    help: &'static str,
    value: Value,
}

/// The metrics of one command's last run, ready to write to its own file.
pub(crate) struct Metrics {
    file: &'static str,
    gauges: Vec<Gauge>,
}

/// Unix seconds, or 0 for a clock before the epoch.
fn unix_seconds(at: SystemTime) -> u64 {
    at.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

impl Metrics {
    /// The last deploy: when it finished, whether it committed or rolled back
    /// (fully or partly), and how long it took.
    pub(crate) fn deploy(
        finished: SystemTime,
        committed: bool,
        rolled_back: bool,
        took: Duration,
    ) -> Self {
        Self {
            file: "fraisier_deploy.prom",
            gauges: vec![
                Gauge {
                    name: "fraisier_deploy_last_timestamp_seconds",
                    help: "When the last deploy finished, in Unix seconds.",
                    value: Value::Count(unix_seconds(finished)),
                },
                Gauge {
                    name: "fraisier_deploy_last_success",
                    help: "Whether the last deploy committed (1) or not (0).",
                    value: Value::Count(u64::from(committed)),
                },
                Gauge {
                    name: "fraisier_deploy_last_rolled_back",
                    help: "Whether the last deploy rolled back, fully or partly (1), or not (0).",
                    value: Value::Count(u64::from(rolled_back)),
                },
                Gauge {
                    name: "fraisier_deploy_last_duration_seconds",
                    help: "How long the last deploy took, in seconds.",
                    value: Value::Seconds(took.as_secs_f64()),
                },
            ],
        }
    }

    /// The last successful backup: when it started and its archive's size.
    pub(crate) fn backup(finished: SystemTime, size_bytes: u64) -> Self {
        Self {
            file: "fraisier_backup.prom",
            gauges: vec![
                Gauge {
                    name: "fraisier_backup_last_success_timestamp_seconds",
                    help: "When the last successful backup started, in Unix seconds.",
                    value: Value::Count(unix_seconds(finished)),
                },
                Gauge {
                    name: "fraisier_backup_last_size_bytes",
                    help: "The size of the last successful backup's archive, in bytes.",
                    value: Value::Count(size_bytes),
                },
            ],
        }
    }

    /// The last successful restore drill: when it started.
    pub(crate) fn drill(finished: SystemTime) -> Self {
        Self {
            file: "fraisier_drill.prom",
            gauges: vec![Gauge {
                name: "fraisier_drill_last_success_timestamp_seconds",
                help: "When the last successful restore drill started, in Unix seconds.",
                value: Value::Count(unix_seconds(finished)),
            }],
        }
    }

    /// The file this command's metrics go to, within the textfile directory.
    pub(crate) const fn file(&self) -> &'static str {
        self.file
    }

    /// The file's text in the Prometheus exposition format.
    pub(crate) fn render(&self, labels: &Labels<'_>) -> String {
        let labels = format!(
            "{{fraise=\"{}\",environment=\"{}\"}}",
            escape(labels.fraise),
            escape(labels.environment)
        );
        let mut text = String::new();
        for gauge in &self.gauges {
            let value = match gauge.value {
                Value::Count(count) => count.to_string(),
                Value::Seconds(seconds) => seconds.to_string(),
            };
            let _ = write!(
                text,
                "# HELP {name} {help}\n# TYPE {name} gauge\n{name}{labels} {value}\n",
                name = gauge.name,
                help = gauge.help,
            );
        }
        text
    }

    /// Write the file into `dir`, replacing the previous one only once the new one
    /// is complete, so a scrape never reads half a file. The temporary name does
    /// not end in `.prom`, so the textfile collector never reads it either.
    pub(crate) fn write(&self, dir: &Path, labels: &Labels<'_>) -> std::io::Result<PathBuf> {
        let path = dir.join(self.file);
        let partial = dir.join(format!(".{}.{}.partial", self.file, std::process::id()));
        std::fs::write(&partial, self.render(labels))?;
        std::fs::rename(&partial, &path).inspect_err(|_| {
            let _ = std::fs::remove_file(&partial);
        })?;
        Ok(path)
    }
}

/// Escape a label value: backslash, double quote and newline.
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::{Labels, Metrics};
    use std::collections::HashSet;
    use std::time::{Duration, SystemTime};

    const LABELS: Labels<'static> = Labels {
        fraise: "checkout",
        environment: "production",
    };

    /// 2026-10-26T03:15:00Z.
    fn at() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_mins(29_883_075)
    }

    fn golden(name: &str) -> &'static str {
        match name {
            "deploy-committed" => include_str!("../tests/fixtures/metrics/deploy-committed.prom"),
            "deploy-rolled-back" => {
                include_str!("../tests/fixtures/metrics/deploy-rolled-back.prom")
            }
            "backup" => include_str!("../tests/fixtures/metrics/backup.prom"),
            "drill" => include_str!("../tests/fixtures/metrics/drill.prom"),
            other => panic!("no golden file {other}"),
        }
    }

    fn is_metric_name(name: &str) -> bool {
        let mut chars = name.chars();
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == ':')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
    }

    /// Parse `{name="value",...}`, returning the label names, or why it is malformed.
    fn parse_labels(text: &str) -> Result<Vec<String>, String> {
        let inner = text
            .strip_prefix('{')
            .and_then(|t| t.strip_suffix('}'))
            .ok_or_else(|| format!("labels not braced: {text}"))?;
        let mut names = Vec::new();
        let mut rest = inner;
        while !rest.is_empty() {
            let (name, after) = rest
                .split_once("=\"")
                .ok_or_else(|| format!("label without =\": {rest}"))?;
            if !is_metric_name(name) || name.contains(':') {
                return Err(format!("bad label name {name:?}"));
            }
            let mut end = None;
            let mut escaped = false;
            for (i, c) in after.char_indices() {
                match (escaped, c) {
                    (true, '\\' | '"' | 'n') => escaped = false,
                    (true, other) => return Err(format!("bad escape \\{other}")),
                    (false, '\\') => escaped = true,
                    (false, '"') => {
                        end = Some(i);
                        break;
                    }
                    (false, '\n') => return Err("raw newline in a label value".to_owned()),
                    _ => {}
                }
            }
            let end = end.ok_or("unterminated label value")?;
            names.push(name.to_owned());
            rest = &after[end + 1..];
            if let Some(next) = rest.strip_prefix(',') {
                rest = next;
            } else if !rest.is_empty() {
                return Err(format!("junk after a label: {rest}"));
            }
        }
        Ok(names)
    }

    /// A strict reading of the text exposition format, as far as these files use
    /// it: every metric has one HELP and then one TYPE `gauge` before its samples,
    /// samples are `name{labels} value` with a finite value, nothing repeats, and
    /// the file ends in a newline with no blank line.
    fn parse_strictly(text: &str) -> Result<Vec<(String, f64)>, String> {
        if !text.ends_with('\n') {
            return Err("no trailing newline".to_owned());
        }
        let mut samples = Vec::new();
        let mut helped = HashSet::new();
        let mut typed = HashSet::new();
        let mut seen_series = HashSet::new();
        for line in text.lines() {
            if line.is_empty() {
                return Err("blank line".to_owned());
            }
            if let Some(rest) = line.strip_prefix("# HELP ") {
                let (name, help) = rest.split_once(' ').ok_or("HELP without text")?;
                if help.is_empty() || !is_metric_name(name) || !helped.insert(name.to_owned()) {
                    return Err(format!("bad or repeated HELP: {line}"));
                }
            } else if let Some(rest) = line.strip_prefix("# TYPE ") {
                let (name, kind) = rest.split_once(' ').ok_or("TYPE without a kind")?;
                if kind != "gauge" || !helped.contains(name) || !typed.insert(name.to_owned()) {
                    return Err(format!("bad, repeated or unhelped TYPE: {line}"));
                }
            } else if line.starts_with('#') {
                return Err(format!("stray comment: {line}"));
            } else {
                let (series, value) = line.rsplit_once(' ').ok_or("sample without a value")?;
                let (name, labels) = series
                    .find('{')
                    .map_or((series, ""), |at| series.split_at(at));
                if !typed.contains(name) || !seen_series.insert(series.to_owned()) {
                    return Err(format!("untyped or repeated sample: {line}"));
                }
                if !labels.is_empty() {
                    let names = parse_labels(labels)?;
                    if names != ["fraise", "environment"] {
                        return Err(format!("unexpected labels {names:?}"));
                    }
                }
                let value: f64 = value.parse().map_err(|_| format!("bad value: {line}"))?;
                if !value.is_finite() {
                    return Err(format!("non-finite value: {line}"));
                }
                samples.push((name.to_owned(), value));
            }
        }
        if samples.is_empty() {
            return Err("no samples".to_owned());
        }
        Ok(samples)
    }

    #[test]
    fn the_golden_files_parse_strictly() {
        for name in ["deploy-committed", "deploy-rolled-back", "backup", "drill"] {
            parse_strictly(golden(name)).unwrap_or_else(|why| panic!("{name}: {why}"));
        }
    }

    #[test]
    fn the_strict_parser_rejects_what_prometheus_would() {
        let good = golden("drill");
        for bad in [
            good.trim_end().to_owned(),
            good.replace("gauge", "counter"),
            good.replace("# HELP", "# HALP"),
            good.replace("1792984500", "NaN"),
            good.replace("\"checkout\"", "\"check\nout\""),
            format!("{good}{}", good.lines().last().unwrap_or_default()),
        ] {
            assert!(parse_strictly(&bad).is_err(), "accepted:\n{bad}");
        }
    }

    #[test]
    fn a_committed_deploy_renders_its_golden_file() {
        let metrics = Metrics::deploy(at(), true, false, Duration::from_millis(42_500));
        assert_eq!(metrics.render(&LABELS), golden("deploy-committed"));
    }

    #[test]
    fn a_rolled_back_deploy_renders_its_golden_file() {
        let metrics = Metrics::deploy(at(), false, true, Duration::from_millis(7_250));
        assert_eq!(metrics.render(&LABELS), golden("deploy-rolled-back"));
    }

    #[test]
    fn a_backup_renders_its_golden_file() {
        assert_eq!(
            Metrics::backup(at(), 1_048_576).render(&LABELS),
            golden("backup")
        );
    }

    #[test]
    fn a_drill_renders_its_golden_file() {
        assert_eq!(Metrics::drill(at()).render(&LABELS), golden("drill"));
    }

    #[test]
    fn label_values_are_escaped() {
        let labels = Labels {
            fraise: "a\"b\\c",
            environment: "x\ny",
        };
        let text = Metrics::drill(at()).render(&labels);
        assert!(
            text.contains(r#"{fraise="a\"b\\c",environment="x\ny"}"#),
            "{text}"
        );
        parse_strictly(&text).unwrap_or_else(|why| panic!("{why}:\n{text}"));
    }

    #[test]
    fn writing_replaces_the_file_and_leaves_nothing_else_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("fraisier_deploy.prom"), "stale").expect("seed");
        let path = Metrics::deploy(at(), true, false, Duration::from_secs(1))
            .write(dir.path(), &LABELS)
            .expect("write");
        assert_eq!(path, dir.path().join("fraisier_deploy.prom"));
        let text = std::fs::read_to_string(&path).expect("read");
        parse_strictly(&text).unwrap_or_else(|why| panic!("{why}:\n{text}"));
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["fraisier_deploy.prom"]);
    }

    #[test]
    fn each_command_writes_its_own_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let deploy = Metrics::deploy(at(), true, false, Duration::from_secs(1))
            .write(dir.path(), &LABELS)
            .expect("deploy");
        let before = std::fs::read_to_string(&deploy).expect("read");
        Metrics::backup(at(), 1)
            .write(dir.path(), &LABELS)
            .expect("backup");
        Metrics::drill(at())
            .write(dir.path(), &LABELS)
            .expect("drill");
        assert_eq!(std::fs::read_to_string(&deploy).expect("read"), before);
        for file in ["fraisier_backup.prom", "fraisier_drill.prom"] {
            assert!(dir.path().join(file).is_file(), "{file}");
        }
    }
}
