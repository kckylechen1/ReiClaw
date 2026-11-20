use reiclaw::config::Config;
use reiclaw::config::schema::{AliasedAgentConfig, CronJobDecl, CronScheduleDecl};
use reiclaw::cron::{JobType, Schedule, get_job, list_jobs, sync_declarative_jobs};
use tempfile::TempDir;

fn test_config(tmp: &TempDir, schedule_cron: Option<String>) -> Config {
    let mut config = Config {
        data_dir: tmp.path().join("data"),
        config_path: tmp.path().join("config.toml"),
        ..Config::default()
    };
    if let Some(expr) = schedule_cron {
        config.cron.insert(
            "daily-check".into(),
            CronJobDecl {
                name: Some("Daily check".into()),
                job_type: "shell".into(),
                schedule: CronScheduleDecl::Cron { expr, tz: None },
                command: Some("printf ok".into()),
                ..Default::default()
            },
        );
    }
    config.agents.insert(
        "maintenance-agent".into(),
        AliasedAgentConfig {
            enabled: true,
            cron_jobs: vec!["daily-check".into()],
            ..Default::default()
        },
    );
    std::fs::create_dir_all(&config.data_dir).unwrap();
    config
}

#[test]
fn declarative_cron_job_synced_when_schedule_set() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp, Some("0 3 * * *".to_string()));

    sync_declarative_jobs(&config, &config.cron).unwrap();

    let job = get_job(&config, "daily-check").unwrap();
    assert_eq!(job.id, "daily-check");
    assert_eq!(job.command, "printf ok");
    assert_eq!(job.source, "declarative");
    assert!(matches!(job.schedule, Schedule::Cron { ref expr, .. } if expr == "0 3 * * *"));
}

#[test]
fn declarative_cron_job_not_synced_when_schedule_none() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp, None);

    sync_declarative_jobs(&config, &config.cron).unwrap();

    let result = get_job(&config, "daily-check");
    assert!(
        result.is_err(),
        "declarative job should not exist when schedule_cron is None"
    );
}

#[test]
fn declarative_cron_job_removed_when_schedule_cleared() {
    let tmp = TempDir::new().unwrap();
    let config_with_schedule = test_config(&tmp, Some("0 3 * * *".to_string()));

    sync_declarative_jobs(&config_with_schedule, &config_with_schedule.cron).unwrap();
    assert!(get_job(&config_with_schedule, "daily-check").is_ok());

    let config_without_schedule = test_config(&tmp, None);
    sync_declarative_jobs(&config_without_schedule, &config_without_schedule.cron).unwrap();

    let result = get_job(&config_without_schedule, "daily-check");
    assert!(
        result.is_err(),
        "declarative job should be removed when schedule_cron is cleared"
    );
}

#[test]
fn declarative_cron_job_schedule_updated() {
    let tmp = TempDir::new().unwrap();
    let config_v1 = test_config(&tmp, Some("0 3 * * *".to_string()));

    sync_declarative_jobs(&config_v1, &config_v1.cron).unwrap();

    let job_v1 = get_job(&config_v1, "daily-check").unwrap();
    let next_run_v1 = job_v1.next_run;

    let config_v2 = test_config(&tmp, Some("0 2 * * *".to_string()));
    sync_declarative_jobs(&config_v2, &config_v2.cron).unwrap();

    let job_v2 = get_job(&config_v2, "daily-check").unwrap();
    assert!(matches!(job_v2.schedule, Schedule::Cron { ref expr, .. } if expr == "0 2 * * *"));
    assert_ne!(
        job_v2.next_run, next_run_v1,
        "next_run should be recalculated when schedule changes"
    );
}

#[test]
fn declarative_cron_job_id_is_stable() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp, Some("0 3 * * *".to_string()));

    for _ in 0..2 {
        sync_declarative_jobs(&config, &config.cron).unwrap();
    }

    let job = get_job(&config, "daily-check").unwrap();
    assert_eq!(job.id, "daily-check");

    let all_jobs = list_jobs(&config).unwrap();
    let matching_jobs: Vec<_> = all_jobs.iter().filter(|j| j.id == "daily-check").collect();
    assert_eq!(
        matching_jobs.len(),
        1,
        "should have exactly one declarative job, not duplicates"
    );
}

#[test]
fn declarative_cron_job_retains_command() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp, Some("0 3 * * *".to_string()));

    sync_declarative_jobs(&config, &config.cron).unwrap();

    let job = get_job(&config, "daily-check").unwrap();
    assert_eq!(job.command, "printf ok");
}

#[test]
fn declarative_cron_job_type_is_shell() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp, Some("0 3 * * *".to_string()));

    sync_declarative_jobs(&config, &config.cron).unwrap();

    let job = get_job(&config, "daily-check").unwrap();
    assert_eq!(job.job_type, JobType::Shell);
}

#[test]
fn declarative_cron_job_source_is_declarative() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp, Some("0 3 * * *".to_string()));

    sync_declarative_jobs(&config, &config.cron).unwrap();

    let job = get_job(&config, "daily-check").unwrap();
    assert_eq!(job.source, "declarative");
}
