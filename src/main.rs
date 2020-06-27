use std::collections::BTreeMap;

use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::signal::unix::{signal, SignalKind};

mod client;
mod prelude;

use crate::prelude::*;

mod error {
    pub type Error = anyhow::Error;
    pub type Result<T> = std::result::Result<T, Error>;
}

pub mod proto {
    pub mod common {
        tonic::include_proto!("common");
    }
    pub mod seabird {
        tonic::include_proto!("seabird");
    }

    pub use self::common::*;
    pub use self::seabird::*;
}

#[derive(serde::Deserialize)]
struct ProxiedChannel {
    source: String,
    target: String,
    user_suffix: Option<String>,
}

#[derive(serde::Deserialize)]
struct ConfigFile {
    proxied_channels: Vec<ProxiedChannel>,
}

async fn read_config(filename: &str) -> Result<BTreeMap<String, Vec<client::ChannelTarget>>> {
    let mut buf = String::new();
    let mut file = File::open(filename).await?;

    file.read_to_string(&mut buf).await?;

    let data: ConfigFile = serde_json::from_str(&buf)?;

    let mut out = BTreeMap::new();

    for channel in data.proxied_channels.into_iter() {
        out.entry(channel.source)
            .or_insert_with(Vec::new)
            .push(client::ChannelTarget::new(
                channel.target,
                channel.user_suffix,
            ));
    }

    Ok(out)
}

#[tokio::main]
async fn main() -> error::Result<()> {
    // Try to load dotenv before loading the logger or trying to set defaults.
    let env_res = dotenv::dotenv();

    // There's a little bit of an oddity here, since we want to set it if it
    // hasn't already been set, but we want this done before the logger is loaded.
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info,seabird-proxy-plugin=debug");
    }

    // Now that everything is set up, load up the logger.
    pretty_env_logger::init_timed();

    // We ignore failures here because we want to fall back to loading from the
    // environment.
    if let Ok(path) = env_res {
        info!("Loaded env from {:?}", path);
    }

    let config_file = dotenv::var("PROXY_CONFIG_FILE")
        .context("Missing $PROXY_CONFIG_FILE. You must specify a config file for the plugin.")?;

    let proxied_channels = read_config(&config_file).await?;

    // Load our config from command line arguments
    let config = client::ClientConfig::new(
        dotenv::var("SEABIRD_HOST")
            .context("Missing $SEABIRD_HOST. You must specify a Seabird host.")?,
        dotenv::var("SEABIRD_TOKEN")
            .context("Missing $SEABIRD_TOKEN. You must specify a valid auth token.")?,
    );

    let client = client::Client::new(config).await?;

    client.set_proxied_channels(proxied_channels).await;

    // Spawn our token reader task
    let mut signal_stream = signal(SignalKind::hangup())?;
    let config_client = client.clone();
    tokio::spawn(async move {
        loop {
            signal_stream.recv().await;

            info!("got SIGHUP, attempting to reload config");

            match read_config(&config_file).await {
                Ok(proxied_channels) => {
                    config_client.set_proxied_channels(proxied_channels).await;
                    info!("reloaded config");
                }
                Err(err) => warn!("failed to reload config: {}", err),
            }
        }
    });

    client.run().await
}
