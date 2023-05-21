#![allow(clippy::collapsible_if)]

pub mod cli;
pub mod client;
pub mod config;
pub mod counters;
pub mod db;
pub mod download;
pub mod encryption;
pub mod ls;
pub mod path;
pub mod pull_updates;
pub mod rules;
pub mod sync;
pub mod term;
pub mod upload;

use crate::{
    ls::{local_status, ls},
    pull_updates::pull_updates,
    upload::upload,
};
use aes_siv::{Aes256SivAead, KeyInit};
use anyhow::{anyhow, Result};
use cli::Cli;
use client::Client;
use config::Config;
use counters::Counters;
use derivative::Derivative;
use download::{download_latest, download_version};
use encryption::encrypt_path;
use path::SanitizedLocalPath;
use rammingen_protocol::endpoints::{MovePath, RemovePath, ResetVersion};
use rules::Rules;
use std::{collections::HashSet, sync::Arc};
use sync::sync;
use term::{clear_status, error, info};

#[derive(Derivative)]
pub struct Ctx {
    pub config: Config,
    pub client: Client,
    #[derivative(Debug = "ignore")]
    pub cipher: Aes256SivAead,
    pub db: crate::db::Db,
    pub counters: Counters,
}

pub async fn run(cli: Cli, config: Config) -> Result<()> {
    let local_db_path = if let Some(v) = &config.local_db_path {
        v.clone()
    } else {
        let data_dir = dirs::data_dir().ok_or_else(|| anyhow!("cannot find config dir"))?;
        data_dir.join("rammingen.db")
    };
    let ctx = Arc::new(Ctx {
        client: Client::new(config.server_url.clone(), &config.token),
        cipher: Aes256SivAead::new(&config.encryption_key.0),
        config,
        db: crate::db::Db::open(&local_db_path)?,
        counters: Counters::default(),
    });
    #[allow(unused_variables)]
    match cli.command {
        cli::Command::Sync => {
            sync(&ctx).await?;
        }
        cli::Command::DryRun => todo!(),
        cli::Command::Upload {
            local_path,
            archive_path,
        } => {
            let local_path = SanitizedLocalPath::new(&local_path)?;
            if let Err(err) = upload(
                &ctx,
                &local_path,
                &archive_path,
                &mut Rules::new(&[&ctx.config.always_exclude], local_path.clone()),
                false,
                &mut HashSet::new(),
            )
            .await
            {
                error(format!("Failed to process {:?}: {:?}", local_path, err));
            }
            clear_status();
            ctx.counters.report();
        }
        cli::Command::Download {
            archive_path,
            local_path,
            version,
        } => {
            if let Some(version) = version {
                download_version(&ctx, &archive_path, &local_path, version.into()).await?;
            } else {
                pull_updates(&ctx).await?;
                download_latest(
                    &ctx,
                    &archive_path,
                    &local_path,
                    &mut Rules::new(&[&ctx.config.always_exclude], local_path.clone()),
                    false,
                )
                .await?;
            }
        }
        cli::Command::LocalStatus { path } => local_status(&ctx, &path).await?,
        cli::Command::Ls { path, deleted } => ls(&ctx, &path, deleted).await?,
        cli::Command::History {
            archive_path,
            time_spec,
        } => todo!(),
        cli::Command::Reset {
            archive_path,
            version,
        } => {
            let stats = ctx
                .client
                .request(&ResetVersion {
                    path: encrypt_path(&archive_path, &ctx.cipher)?,
                    recorded_at: version.into(),
                })
                .await?;
            info(format!("{:?}", stats));
        }
        cli::Command::Move { old_path, new_path } => {
            let stats = ctx
                .client
                .request(&MovePath {
                    old_path: encrypt_path(&old_path, &ctx.cipher)?,
                    new_path: encrypt_path(&new_path, &ctx.cipher)?,
                })
                .await?;
            info(format!("{:?}", stats));
        }
        cli::Command::Remove { archive_path } => {
            let stats = ctx
                .client
                .request(&RemovePath {
                    path: encrypt_path(&archive_path, &ctx.cipher)?,
                })
                .await?;
            info(format!("{:?}", stats));
        }
    }

    #[allow(unreachable_code)]
    Ok(())
}

#[cfg(target_family = "unix")]
pub fn unix_mode(metadata: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::prelude::PermissionsExt;

    Some(metadata.permissions().mode())
}

#[cfg(not(target_family = "unix"))]
pub fn unix_mode(_metadata: &Metadata) -> Option<u32> {
    None
}
