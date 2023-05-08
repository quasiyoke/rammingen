mod diff;
mod shuffle;

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;
use chrono::{DateTime, FixedOffset, Utc};
use diff::{diff, diff_ignored, is_leftover_dir_with_ignored_files};
use fs_err::{
    copy, create_dir, create_dir_all, read_dir, remove_dir_all, remove_file, rename, write,
};
use portpicker::pick_unused_port;
use rammingen::{
    cli::{Cli, Command},
    config::{EncryptionKey, MountPoint},
    path::SanitizedLocalPath,
    rules::Rule,
    term::{clear_status, debug, error, info},
};
use rammingen_protocol::{util::native_to_archive_relative_path, ArchivePath};
use rand::{seq::SliceRandom, thread_rng, Rng};
use shuffle::{choose_path, random_content, random_name, shuffle};
use sqlx::{query, PgPool};
use tempfile::TempDir;
use tokio::time::sleep;

fn copy_dir_all(src: &Path, dst: impl AsRef<Path>) -> Result<()> {
    create_dir_all(&dst)?;
    for entry in read_dir(src)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), dst.as_ref().join(entry.file_name()))?;
        } else {
            fs_err::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let r = try_main().await;
    clear_status();
    if let Err(err) = r {
        error(format!("{:?}", err));
    }
}

async fn try_main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let database_url = std::env::args().nth(1).expect("missing arg");
    rammingen_server::migrate(&database_url).await?;

    // TODO: remove into_path
    let dir = TempDir::new()?.into_path();
    debug(format!("dir: {}", dir.display()));
    let storage_path = dir.join("storage");
    create_dir_all(&storage_path)?;

    let port = pick_unused_port().expect("failed to pick port");
    let server_config = rammingen_server::Config {
        bind_addr: SocketAddr::new("127.0.0.1".parse()?, port),
        database_url: database_url.clone(),
        storage_path,
    };

    let encryption_key = EncryptionKey::generate();
    let db_pool = PgPool::connect(&database_url).await?;
    let mut clients = Vec::new();
    let archive_mount_path: ArchivePath = "ar:/my_files".parse()?;
    for client_index in 0..3 {
        let client_dir = dir.join(format!("client{client_index}"));
        let mount_dir = client_dir.join("mount1");
        create_dir_all(&mount_dir)?;
        let token = format!("token{client_index}");
        let config = rammingen::config::Config {
            always_exclude: vec![
                Rule::NameEquals("target".into()),
                Rule::NameMatches("^build_".parse()?),
            ],
            mount_points: vec![MountPoint {
                local_path: mount_dir.to_str().unwrap().parse()?,
                archive_path: archive_mount_path.clone(),
                exclude: vec![],
            }],
            encryption_key: encryption_key.clone(),
            server_url: format!("http://127.0.0.1:{port}/"),
            token: token.clone(),
            salt: "salt1".into(),
            local_db_path: Some(client_dir.join("db")),
        };
        clients.push(ClientData { config, mount_dir });

        query("INSERT INTO sources(name, secret) VALUES ($1, $2)")
            .bind(format!("client{client_index}"))
            .bind(token)
            .execute(&db_pool)
            .await?;
    }

    tokio::spawn(async move {
        if let Err(err) = rammingen_server::run(server_config).await {
            clear_status();
            error(format!("server failed: {err:?}"));
            std::process::exit(1);
        }
    });

    let snapshot_for_download_version_path = dir.join("snapshot_for_download_version");
    let mut snapshot_time: Option<DateTime<Utc>> = None;
    for _ in 0..1000 {
        if thread_rng().gen_bool(0.2) {
            // mutate through server command
            let expected = dir.join("expected");
            if expected.exists() {
                remove_dir_or_file(&expected)?;
            }
            copy_dir_all(&clients[0].mount_dir, &expected)?;
            let client1 = clients.choose(&mut thread_rng()).unwrap();
            if thread_rng().gen_bool(0.3) {
                // upload new path
                let path_for_upload = dir.join("for_upload");
                if path_for_upload.exists() {
                    remove_dir_or_file(&path_for_upload)?;
                }
                if thread_rng().gen_bool(0.3) {
                    write(&path_for_upload, random_content())?;
                } else {
                    create_dir(&path_for_upload)?;
                    shuffle(&path_for_upload)?;
                }
                let parent_path = choose_path(&expected, false, true, true, false)?.unwrap();
                let path_in_expected = parent_path.join(random_name(false));
                if path_in_expected.exists() {
                    continue;
                }
                if path_for_upload.is_dir() {
                    copy_dir_all(&path_for_upload, &path_in_expected)?;
                } else {
                    copy(&path_for_upload, &path_in_expected)?;
                }
                let archive_path =
                    archive_subpath(&archive_mount_path, &expected, &path_in_expected)?;
                debug(format!("Checking upload ({archive_path})"));
                client1
                    .upload(SanitizedLocalPath::new(&path_for_upload)?, archive_path)
                    .await?;
            } else if thread_rng().gen_bool(0.5) {
                // move path
                let Some(path1) = choose_path(&expected, true, true, false, false)? else {
                    continue;
                };
                let path2_parent = choose_path(&expected, false, true, true, false)?.unwrap();
                let path2 = path2_parent.join(random_name(false));
                if path2.exists() || path2.starts_with(&path1) {
                    continue;
                }
                rename(&path1, &path2)?;
                let archive_path = archive_subpath(&archive_mount_path, &expected, &path1)?;
                let new_archive_path = archive_subpath(&archive_mount_path, &expected, &path2)?;
                debug(format!(
                    "Checking mv ({archive_path} -> {new_archive_path})"
                ));
                client1.move_path(archive_path, new_archive_path).await?;
            } else {
                // remove path
                let Some(path1) = choose_path(&expected, true, true, false, false)? else {
                    continue;
                };
                remove_dir_or_file(&path1)?;
                let archive_path = archive_subpath(&archive_mount_path, &expected, &path1)?;
                debug(format!("Checking rm {archive_path}"));
                client1.remove_path(archive_path).await?;
            }
            for client in &clients {
                client.sync().await?;
                diff(&expected, &client.mount_dir)?;
            }
        } else {
            // edit mount
            let index = thread_rng().gen_range(0..clients.len());
            for _ in 0..thread_rng().gen_range(1..=3) {
                debug(format!("shuffling mount for client {index}"));
                shuffle(&clients[index].mount_dir)?;
                debug(format!("syncing client {index}"));
                clients[index].sync().await?;
            }
            for (index2, client) in clients.iter().enumerate() {
                if index2 != index {
                    debug(format!("syncing client {index2}"));
                    let before_sync_snapshot = dir.join("snapshot");
                    if before_sync_snapshot.exists() {
                        remove_dir_all(&before_sync_snapshot)?;
                    }
                    copy_dir_all(&client.mount_dir, &before_sync_snapshot)?;
                    client.sync().await?;
                    diff_ignored(&client.mount_dir, &before_sync_snapshot)?;
                }
            }
            for client in &clients[1..] {
                diff(&clients[0].mount_dir, &client.mount_dir)?;
            }
        }
        check_download(
            &dir,
            &archive_mount_path,
            &clients,
            None,
            &clients.choose(&mut thread_rng()).unwrap().mount_dir,
        )
        .await?;
        if thread_rng().gen_bool(0.3) {
            if let Some(snapshot_time_value) = snapshot_time {
                check_download(
                    &dir,
                    &archive_mount_path,
                    &clients,
                    Some(snapshot_time_value),
                    &snapshot_for_download_version_path,
                )
                .await?;
                snapshot_time = None;
            } else {
                info("Saving snapshot for download_version test");
                sleep(Duration::from_millis(500)).await;
                snapshot_time = Some(Utc::now());
                if snapshot_for_download_version_path.exists() {
                    remove_dir_or_file(&snapshot_for_download_version_path)?;
                }
                copy_dir_all(&clients[0].mount_dir, &snapshot_for_download_version_path)?;
                sleep(Duration::from_millis(500)).await;
            }
        }
    }

    Ok(())
}

struct ClientData {
    mount_dir: PathBuf,
    config: rammingen::config::Config,
}

impl ClientData {
    async fn sync(&self) -> Result<()> {
        rammingen::run(
            Cli {
                config: None,
                command: Command::Sync,
            },
            self.config.clone(),
        )
        .await
    }
    async fn download(
        &self,
        archive_path: ArchivePath,
        local_path: SanitizedLocalPath,
        version: Option<DateTime<FixedOffset>>,
    ) -> Result<()> {
        rammingen::run(
            Cli {
                config: None,
                command: Command::Download {
                    archive_path,
                    local_path,
                    version,
                },
            },
            self.config.clone(),
        )
        .await
    }
    async fn upload(
        &self,
        local_path: SanitizedLocalPath,
        archive_path: ArchivePath,
    ) -> Result<()> {
        rammingen::run(
            Cli {
                config: None,
                command: Command::Upload {
                    local_path,
                    archive_path,
                },
            },
            self.config.clone(),
        )
        .await
    }
    async fn move_path(
        &self,
        archive_path: ArchivePath,
        new_archive_path: ArchivePath,
    ) -> Result<()> {
        rammingen::run(
            Cli {
                config: None,
                command: Command::Move {
                    old_path: archive_path,
                    new_path: new_archive_path,
                },
            },
            self.config.clone(),
        )
        .await
    }
    async fn remove_path(&self, archive_path: ArchivePath) -> Result<()> {
        rammingen::run(
            Cli {
                config: None,
                command: Command::Remove { archive_path },
            },
            self.config.clone(),
        )
        .await
    }
}

fn archive_subpath(
    archive_root_path: &ArchivePath,
    local_root_path: &Path,
    path: &Path,
) -> Result<ArchivePath> {
    if path == local_root_path {
        Ok(archive_root_path.clone())
    } else {
        let relative = path.strip_prefix(local_root_path)?;
        archive_root_path.join_multiple(&native_to_archive_relative_path(relative)?)
    }
}

async fn check_download(
    dir: &Path,
    archive_mount_path: &ArchivePath,
    clients: &[ClientData],
    version: Option<DateTime<Utc>>,
    source_dir: &Path,
) -> Result<()> {
    let local_path = choose_path(source_dir, true, true, true, false)?.unwrap();
    if is_leftover_dir_with_ignored_files(&local_path)? {
        return Ok(());
    }
    let archive_path = archive_subpath(archive_mount_path, source_dir, &local_path)?;
    info(format!(
        "Checking download: {}, {:?}",
        archive_path, version
    ));
    let client2 = clients.choose(&mut thread_rng()).unwrap();
    let destination = dir.join("tmp_download");
    if destination.exists() {
        remove_dir_or_file(&destination)?;
    }
    client2
        .download(
            archive_path,
            destination.to_str().unwrap().parse()?,
            version.map(Into::into),
        )
        .await?;
    diff(&local_path, &destination)?;
    Ok(())
}

fn is_ignored(path: &Path) -> bool {
    let name = path.file_name().unwrap().to_str().unwrap();
    name == "target" || name.starts_with("build_")
}

fn remove_dir_or_file(path: &Path) -> Result<()> {
    if path.is_dir() {
        remove_dir_all(path)?;
    } else {
        remove_file(path)?;
    }
    Ok(())
}
