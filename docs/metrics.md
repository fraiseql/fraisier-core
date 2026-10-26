# Metrics

fraisier is a one-shot CLI: it starts, does one thing, and exits. It has no
process that Prometheus can scrape. It cannot report `up`, and it cannot keep a
counter across runs. Instead, each command writes a small file for
node_exporter's
[textfile collector](https://github.com/prometheus/node_exporter#textfile-collector)
to serve:

```toml
[metrics]
textfile_dir = "/var/lib/node_exporter/textfile"
```

Point node_exporter at the same directory with
`--collector.textfile.directory=/var/lib/node_exporter/textfile`. `textfile_dir`
must be absolute. Without a `[metrics]` section, no file is written.

## One file per command

| File | Written by | When |
|---|---|---|
| `fraisier_deploy.prom` | `fraisier deploy` | after every deploy that reaches an outcome, committed or not (never on `--dry-run`) |
| `fraisier_backup.prom` | `fraisier backup` | after a successful backup only |
| `fraisier_drill.prom` | `fraisier backup drill` | after a passed drill only |

Each command writes only its own file, so a backup never overwrites the last
deploy's metrics. A file is written beside its final name and renamed into place,
so a scrape never reads half a file. The temporary name does not end in `.prom`,
so the collector skips it.

A failed backup or drill leaves its file as it was. Its success timestamp keeps
telling the truth, and a staleness alert fires as soon as it is old enough. A file
that cannot be written is a warning in the command's output. It never fails the
command, because the deploy or backup it describes has already happened.

## The metrics

Every metric is a gauge describing the last run. Every sample carries exactly
two labels, `fraise` and `environment`, taken from `[deploy]`.

| Metric | Meaning |
|---|---|
| `fraisier_deploy_last_timestamp_seconds` | When the last deploy finished, in Unix seconds. |
| `fraisier_deploy_last_success` | 1 if the last deploy committed, else 0. |
| `fraisier_deploy_last_rolled_back` | 1 if the last deploy rolled back, fully or partly, else 0. |
| `fraisier_deploy_last_duration_seconds` | How long the last deploy took. |
| `fraisier_backup_last_success_timestamp_seconds` | When the last successful backup started. For an archive in `[backup].dir`, this is the time in its name. |
| `fraisier_backup_last_size_bytes` | The size of the last successful backup's archive. |
| `fraisier_drill_last_success_timestamp_seconds` | When the last passed restore drill started. |

A partial rollback reports `fraisier_deploy_last_rolled_back = 1` too. It is the
worse outcome, because a compensation failed and the host needs an operator.
`fraisier_deploy_last_success = 0` tells the two failures apart from a commit;
the deploy's own output and state store tell them apart from each other.

Example `fraisier_backup.prom`:

```
# HELP fraisier_backup_last_success_timestamp_seconds When the last successful backup started, in Unix seconds.
# TYPE fraisier_backup_last_success_timestamp_seconds gauge
fraisier_backup_last_success_timestamp_seconds{fraise="checkout",environment="production"} 1792984500
# HELP fraisier_backup_last_size_bytes The size of the last successful backup's archive, in bytes.
# TYPE fraisier_backup_last_size_bytes gauge
fraisier_backup_last_size_bytes{fraise="checkout",environment="production"} 1048576
```

## Alerting on them

Because the metrics are timestamps, staleness is a comparison with `time()`. For
example, a backup alert that fires once the newest good backup is a day old looks
like this:

```yaml
- alert: BackupStale
  expr: time() - fraisier_backup_last_success_timestamp_seconds > 24 * 3600
```

Use the same threshold as the drill's `[drill].max_age_hours` so the two agree.
