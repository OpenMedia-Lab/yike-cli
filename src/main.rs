use anyhow::{Context, Result, bail, ensure};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;
use yike_cli::{api::Api, auth, download, model::*, planner, state::Store, upload};

#[derive(Parser)]
#[command(
    name = "yike",
    version,
    about = "Resumable original-media transfers for your own Baidu Photo library"
)]
struct Cli {
    /// Private native filesystem directory; defaults to platform local data/yike
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    /// Compact JSON stdout; progress always uses stderr
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Doctor {
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        target: Option<PathBuf>,
    },
    #[command(subcommand)]
    Auth(AuthCommand),
    Capabilities,
    #[command(subcommand)]
    Albums(AlbumCommand),
    #[command(subcommand)]
    Files(FileCommand),
    #[command(subcommand)]
    Upload(UploadCommand),
    #[command(subcommand)]
    Download(DownloadCommand),
    #[command(subcommand)]
    Jobs(JobCommand),
    /// Cloud original readback against the immutable source SHA-256
    Verify {
        job: String,
    },
    /// Complete local manifest and per-item receipts, including unresolved items
    Report {
        job: String,
    },
    Completions {
        shell: clap_complete::Shell,
    },
}
#[derive(Subcommand)]
enum AuthCommand {
    Login {
        #[arg(long, default_value_t = 19227)]
        port: u16,
        #[arg(long)]
        attach: bool,
    },
    Status {
        #[arg(long)]
        offline: bool,
    },
    Logout,
}
#[derive(Subcommand)]
enum AlbumCommand {
    List,
    Create {
        title: String,
    },
    /// Resolve an uncertain album creation using a reviewed stable ID
    Bind {
        #[arg(long)]
        title: String,
        #[arg(long)]
        id: String,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum MediaKind {
    Video,
    Photo,
}
#[derive(clap::Args, Default)]
struct Filter {
    #[arg(long)]
    album: Option<String>,
    #[arg(long)]
    kind: Option<MediaKind>,
    /// Cloud creation date, UTC YYYY-MM-DD (not camera shooting date)
    #[arg(long,value_parser=parse_date)]
    after: Option<i64>,
    #[arg(long,value_parser=parse_date)]
    before: Option<i64>,
}
#[derive(Subcommand)]
enum FileCommand {
    List {
        #[command(flatten)]
        filter: Filter,
    },
    Search {
        query: String,
        #[command(flatten)]
        filter: Filter,
    },
    Info {
        id: String,
    },
}
#[derive(Subcommand)]
enum UploadCommand {
    /// Immutable MP4 inventory and hashes; no cloud writes
    Plan {
        root: PathBuf,
        #[arg(long)]
        album: String,
        #[arg(long)]
        max_files: Option<usize>,
        #[arg(long)]
        expect_files: Option<usize>,
        /// Explicit attempt ceiling in bytes for an unlimited account; service acceptance is unproven
        #[arg(long, value_name = "BYTES", value_parser = clap::value_parser!(u64).range(1..))]
        attempt_upload_limit: Option<u64>,
    },
    Run {
        plan: String,
    },
}
#[derive(Subcommand)]
enum DownloadCommand {
    Plan {
        #[arg(long="id",num_args=1..,conflicts_with_all=["album","query"])]
        ids: Vec<String>,
        #[arg(long,conflicts_with_all=["ids","query"])]
        album: Option<String>,
        #[arg(long,conflicts_with_all=["ids","album"])]
        query: Option<String>,
        #[arg(long)]
        to: PathBuf,
    },
    Run {
        plan: String,
    },
}
#[derive(Subcommand)]
enum JobCommand {
    List,
    Status { job: String },
    Resume { job: String },
    Retry { job: String },
    Cancel { job: String },
}

fn parse_date(v: &str) -> std::result::Result<i64, String> {
    chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d")
        .map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp())
        .map_err(|_| "expected UTC YYYY-MM-DD".into())
}
fn state_directory(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let path = match explicit.or_else(|| std::env::var_os("YIKE_STATE_DIR").map(PathBuf::from)) {
        Some(path) => path,
        None => directories::BaseDirs::new()
            .context("cannot locate local data directory")?
            .data_local_dir()
            .join("yike"),
    };
    yike_cli::state::require_native_path(&path)?;
    Ok(path)
}
fn api(path: &Path) -> Result<Api> {
    Api::new(auth::load_session(path)?)
}
async fn album(api: &Api, id: &str) -> Result<Album> {
    api.albums()
        .await?
        .into_iter()
        .find(|a| a.id == id)
        .context("album ID not found")
}
async fn files(api: &Api, f: &Filter, q: Option<&str>) -> Result<Vec<RemoteFile>> {
    let a = match &f.album {
        Some(id) => Some(album(api, id).await?),
        None => None,
    };
    ensure!(q.is_none_or(|s| !s.trim().is_empty()), "empty search query");
    if let (Some(after), Some(before)) = (f.after, f.before) {
        ensure!(after <= before, "after exceeds before");
    }
    let q = q.map(str::to_lowercase);
    Ok(api
        .files(a.as_ref())
        .await?
        .into_iter()
        .filter(|v| {
            let n = v.name().to_lowercase();
            let video = [
                ".mp4", ".mov", ".m4v", ".avi", ".mkv", ".mts", ".3gp", ".webm",
            ]
            .iter()
            .any(|e| n.ends_with(e));
            q.as_ref().is_none_or(|q| n.contains(q))
                && f.kind.is_none_or(|k| match k {
                    MediaKind::Video => video,
                    MediaKind::Photo => !video,
                })
                && f.after.is_none_or(|d| v.ctime >= d)
                && f.before.is_none_or(|d| v.ctime < d.saturating_add(86400))
        })
        .collect())
}
fn job(store: &Store, id: &str) -> Result<JobRecord> {
    store.get("jobs", id)?.context("job ID not found")
}
fn summary(store: &Store, j: &JobRecord) -> Result<JobSummary> {
    match j.kind.as_str() {
        "upload" => upload::summary(store, &j.id),
        "download" => download::summary(store, &j.id),
        _ => bail!("unknown job kind"),
    }
}

fn target_diagnostics(target: &Path) -> Result<Value> {
    ensure!(
        target.is_dir(),
        "doctor target must be an existing directory"
    );
    let probe = target.join(format!(".yike-write-probe-{}", uuid::Uuid::new_v4()));
    let writable = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .is_ok_and(|file| {
            let synced = file.sync_all().is_ok();
            drop(file);
            let removed = std::fs::remove_file(&probe).is_ok();
            synced && removed
        });
    Ok(json!({"available_bytes":fs2::available_space(target)?,"writable":writable}))
}
async fn resume(
    store: &Store,
    api: &Api,
    id: &str,
    cancel: CancellationToken,
) -> Result<JobSummary> {
    let mut j = job(store, id)?;
    ensure!(j.account == api.account(), "job belongs to another account");
    j.cancelled = false;
    store.put("jobs", id, &j)?;
    match j.kind.as_str() {
        "upload" => {
            let p = store
                .get("upload_plans", &j.plan_id)?
                .context("upload plan missing")?;
            upload::run(store, api, &p, Some(id), cancel, true).await
        }
        "download" => {
            let p = store
                .get("download_plans", &j.plan_id)?
                .context("download plan missing")?;
            download::run(store, api, &p, Some(id), cancel).await
        }
        _ => bail!("unknown job kind"),
    }
}
fn inventory(root: &Path, expected: usize) -> Result<()> {
    let mut count = 0;
    for e in walkdir::WalkDir::new(root).follow_links(false) {
        let e = e.context("unreadable inventory entry")?;
        if e.path()
            .extension()
            .is_some_and(|s| s.eq_ignore_ascii_case("mp4"))
        {
            ensure!(
                e.file_type().is_file(),
                "MP4 inventory contains nonregular file"
            );
            count += 1;
        }
    }
    ensure!(
        count == expected,
        "inventory differs: expected {expected}, found {count}; review scope before planning"
    );
    eprintln!("Inventory confirmed: {count} MP4 originals");
    Ok(())
}
async fn execute(cli: Cli) -> Result<i32> {
    if let Command::Completions { shell } = &cli.command {
        clap_complete::generate(*shell, &mut Cli::command(), "yike", &mut std::io::stdout());
        return Ok(0);
    }
    let dir = state_directory(cli.state_dir)?;
    let store = Store::open(&dir.join("state.sqlite3"))?;
    let session_path = dir.join("session.json");
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let handler = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("Stopping safely; progress is preserved.");
            signal.cancel();
        }
    });
    let mut exit = 0;
    let output = match cli.command {
        Command::Doctor { offline, target } => {
            let target = target.as_deref().map(target_diagnostics).transpose()?;
            if target.as_ref().is_some_and(|v| v["writable"] == false) {
                exit = 2;
            }
            let verified = store
                .list::<UploadProgress>("upload_items")?
                .iter()
                .filter(|(_, p)| p.state == ItemState::Verified && p.verified_sha256.is_some())
                .count();
            json!({"platform":std::env::consts::OS,"architecture":std::env::consts::ARCH,"state_directory":dir,"state_available_bytes":fs2::available_space(&dir)?,"target":target,"session_saved":session_path.is_file(),"capabilities":if offline{None}else{Some(api(&session_path)?.capabilities().await?)},"validated_web_upload_limit":WEB_UPLOAD_LIMIT,"verification":"original_stream_sha256_against_immutable_source_plan","original_and_full_hash_probe":if verified>0{"previously_verified_on_recorded_objects"}else{"unknown_until_live_readback"},"local_verified_receipts":verified})
        }
        Command::Auth(c) => match c {
            AuthCommand::Login { port, attach } => {
                let s = auth::login(&session_path, port, attach).await?;
                let caps = Api::new(s.clone())?.capabilities().await?;
                json!({"authenticated":true,"account":s.fingerprint(),"capabilities":caps})
            }
            AuthCommand::Status { offline } => {
                let s = auth::load_session(&session_path)?;
                if !offline {
                    Api::new(s.clone())?.capabilities().await?;
                }
                json!({"session_saved":true,"account":s.fingerprint(),"checked_online":!offline})
            }
            AuthCommand::Logout => {
                auth::logout(&session_path)?;
                json!({"session_saved":false})
            }
        },
        Command::Capabilities => serde_json::to_value(api(&session_path)?.capabilities().await?)?,
        Command::Albums(c) => {
            let api = api(&session_path)?;
            match c {
                AlbumCommand::List => serde_json::to_value(api.albums().await?)?,
                AlbumCommand::Create { title } => {
                    serde_json::to_value(upload::ensure_album(&store, &api, &title).await?)?
                }
                AlbumCommand::Bind { title, id } => {
                    let a = album(&api, &id).await?;
                    upload::bind_album(&store, &api.account(), &title, &a)?;
                    serde_json::to_value(a)?
                }
            }
        }
        Command::Files(c) => {
            let api = api(&session_path)?;
            match c {
                FileCommand::List { filter } => {
                    serde_json::to_value(files(&api, &filter, None).await?)?
                }
                FileCommand::Search { query, filter } => {
                    serde_json::to_value(files(&api, &filter, Some(&query)).await?)?
                }
                FileCommand::Info { id } => serde_json::to_value(
                    api.files(None)
                        .await?
                        .into_iter()
                        .find(|v| v.fsid == id)
                        .context("file ID not found")?,
                )?,
            }
        }
        Command::Upload(c) => {
            let api = api(&session_path)?;
            match c {
                UploadCommand::Plan {
                    root,
                    album,
                    max_files,
                    expect_files,
                    attempt_upload_limit,
                } => {
                    ensure!(max_files != Some(0), "max-files must be positive");
                    if let Some(n) = expect_files {
                        inventory(&root, n)?;
                    }
                    let p = planner::build_with_attempt_limit(
                        &store,
                        &api,
                        &root,
                        &album,
                        max_files,
                        cancel.clone(),
                        attempt_upload_limit,
                    )
                    .await?;
                    let blocked:Vec<_>=p.files.iter().filter(|f|f.blocked.is_some()).map(|f|json!({"source":f.relative_path,"bytes":f.stamp.len,"reason":f.blocked})).collect();
                    json!({"plan_id":p.id,"total":p.files.len(),"eligible":p.files.len()-blocked.len(),"bytes":p.files.iter().map(|f|f.stamp.len).sum::<u64>(),"upload_limit":p.upload_limit,"attempt_upload_limit":p.attempt_upload_limit,"blocked":blocked,"cloud_writes":false})
                }
                UploadCommand::Run { plan } => {
                    let p = store
                        .get("upload_plans", &plan)?
                        .context("upload plan ID not found")?;
                    let s = upload::run(&store, &api, &p, None, cancel.clone(), true).await?;
                    if !s.complete {
                        exit = 3;
                    }
                    serde_json::to_value(s)?
                }
            }
        }
        Command::Download(c) => {
            let api = api(&session_path)?;
            match c {
                DownloadCommand::Plan {
                    ids,
                    album,
                    query,
                    to,
                } => {
                    ensure!(
                        !ids.is_empty() || album.is_some() || query.is_some(),
                        "choose explicit IDs, album or query"
                    );
                    let files = if !ids.is_empty() {
                        let wanted: HashSet<_> = ids.iter().collect();
                        ensure!(wanted.len() == ids.len(), "duplicate file IDs");
                        let items: Vec<_> = api
                            .files(None)
                            .await?
                            .into_iter()
                            .filter(|f| wanted.contains(&f.fsid))
                            .collect();
                        ensure!(items.len() == wanted.len(), "some IDs not found");
                        items
                    } else {
                        files(
                            &api,
                            &Filter {
                                album,
                                ..Default::default()
                            },
                            query.as_deref(),
                        )
                        .await?
                    };
                    ensure!(!files.is_empty(), "empty download selection");
                    std::fs::create_dir_all(&to).context("cannot create target")?;
                    let p = DownloadPlan {
                        schema: "yike-download-plan-v1".into(),
                        id: uuid::Uuid::new_v4().to_string(),
                        account: api.account(),
                        target: to.canonicalize()?,
                        created_at: chrono::Utc::now().to_rfc3339(),
                        files,
                    };
                    store.insert("download_plans", &p.id, &p)?;
                    json!({"plan_id":p.id,"total":p.files.len(),"bytes":p.files.iter().map(|f|f.size).sum::<u64>(),"target":p.target,"media_downloaded":false})
                }
                DownloadCommand::Run { plan } => {
                    let p = store
                        .get("download_plans", &plan)?
                        .context("download plan ID not found")?;
                    let s = download::run(&store, &api, &p, None, cancel.clone()).await?;
                    if !s.complete {
                        exit = 3;
                    }
                    serde_json::to_value(s)?
                }
            }
        }
        Command::Jobs(c) => match c {
            JobCommand::List => {
                let mut rows = Vec::new();
                for (_, j) in store.list::<JobRecord>("jobs")? {
                    rows.push(json!({"job":j,"status":summary(&store,&j)?}));
                }
                Value::Array(rows)
            }
            JobCommand::Status { job: id } => {
                let j = job(&store, &id)?;
                json!({"job":j,"status":summary(&store,&j)?})
            }
            JobCommand::Cancel { job: id } => {
                let mut j = job(&store, &id)?;
                j.cancelled = true;
                store.put("jobs", &id, &j)?;
                json!({"job_id":id,"cancel_requested":true,"remote_deleted":false,"source_deleted":false})
            }
            JobCommand::Resume { job: id } | JobCommand::Retry { job: id } => {
                let api = api(&session_path)?;
                let s = resume(&store, &api, &id, cancel.clone()).await?;
                if !s.complete {
                    exit = 3;
                }
                serde_json::to_value(s)?
            }
        },
        Command::Verify { job } => {
            let api = api(&session_path)?;
            ensure!(
                self::job(&store, &job)?.kind == "upload",
                "verify needs an upload job with source SHA-256"
            );
            let s = upload::verify_job(&store, &api, &job, cancel.clone()).await?;
            if !s.complete {
                exit = 3;
            }
            serde_json::to_value(s)?
        }
        Command::Report { job: id } => {
            let j = job(&store, &id)?;
            let (p, items) = if j.kind == "upload" {
                let p: UploadPlan = store
                    .get("upload_plans", &j.plan_id)?
                    .context("plan missing")?;
                let rows: Vec<_> = store
                    .list::<UploadProgress>("upload_items")?
                    .into_iter()
                    .filter(|(_, v)| v.job_id == id)
                    .map(|(_, v)| v)
                    .collect();
                (serde_json::to_value(p)?, serde_json::to_value(rows)?)
            } else {
                let p: DownloadPlan = store
                    .get("download_plans", &j.plan_id)?
                    .context("plan missing")?;
                let rows: Vec<_> = store
                    .list::<DownloadProgress>("download_items")?
                    .into_iter()
                    .filter(|(_, v)| v.job_id == id)
                    .map(|(_, v)| v)
                    .collect();
                (serde_json::to_value(p)?, serde_json::to_value(rows)?)
            };
            json!({"schema":"yike-transfer-report-v1","job":j,"summary":summary(&store,&j)?,"plan":p,"items":items,"original_deletion_authorized":false})
        }
        Command::Completions { .. } => unreachable!(),
    };
    handler.abort();
    if cli.json {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&output)?);
    }
    if cancel.is_cancelled() {
        Ok(130)
    } else {
        Ok(exit)
    }
}
#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let compact = cli.json;
    match execute(cli).await {
        Ok(n) => std::process::exit(n),
        Err(e) => {
            if compact {
                eprintln!("{}", json!({"error":e.to_string(),"complete":false}));
            } else {
                eprintln!("Error: {e}");
            }
            std::process::exit(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn upload_plan_accepts_an_explicit_large_file_attempt_ceiling() {
        assert!(
            Cli::try_parse_from([
                "yike",
                "upload",
                "plan",
                "/fixtures/originals",
                "--album",
                "Originals",
                "--attempt-upload-limit",
                "6442450944"
            ])
            .is_ok()
        );
    }
    #[test]
    fn upload_attempt_ceiling_rejects_zero_negative_and_overflow() {
        for value in ["0", "-1", "18446744073709551616", "5GiB"] {
            assert!(
                Cli::try_parse_from([
                    "yike",
                    "upload",
                    "plan",
                    "/fixtures/originals",
                    "--album",
                    "Originals",
                    "--attempt-upload-limit",
                    value,
                ])
                .is_err(),
                "accepted {value}"
            );
        }
    }
    #[test]
    fn doctor_probes_writability_without_leaving_files() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            target_diagnostics(directory.path()).unwrap()["writable"],
            true
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let ordinary_file = directory.path().join("file");
        std::fs::write(&ordinary_file, b"preserve").unwrap();
        assert!(target_diagnostics(&ordinary_file).is_err());
        assert_eq!(std::fs::read(&ordinary_file).unwrap(), b"preserve");
    }
    #[test]
    fn clap_contract() {
        Cli::command().debug_assert();
    }
    #[test]
    fn no_delete() {
        assert!(Cli::try_parse_from(["yike", "delete", "x"]).is_err());
    }
    #[test]
    fn dates() {
        assert!(parse_date("2000-01-02").is_ok());
        assert!(parse_date("07/23/26").is_err());
    }
    #[test]
    fn unambiguous_selection() {
        assert!(
            Cli::try_parse_from([
                "yike", "download", "plan", "--id", "1", "--album", "2", "--to", "/tmp/out"
            ])
            .is_err()
        );
    }
}
