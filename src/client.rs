use std::collections::{BTreeMap, HashMap};

use http::Uri;
use tokio::sync::{Mutex, RwLock};
use tonic::{
    metadata::{Ascii, MetadataValue},
    transport::{Channel, ClientTlsConfig},
};

use crate::prelude::*;

use crate::proto::seabird::seabird_client::SeabirdClient;

#[derive(Debug)]
pub struct ClientConfig {
    pub url: String,
    pub token: String,
}

impl ClientConfig {
    pub fn new(url: String, token: String) -> Self {
        ClientConfig { url, token }
    }
}

#[derive(Debug)]
pub struct ChannelTarget {
    id: String,
    user_suffix: Option<String>,
}

impl ChannelTarget {
    pub fn new(id: String, user_suffix: Option<String>) -> Self {
        ChannelTarget { id, user_suffix }
    }
}

// Client represents the running bot.
#[derive(Debug)]
pub struct Client {
    config: ClientConfig,
    inner: Mutex<SeabirdClient<tonic::transport::Channel>>,
    proxied_channels: RwLock<BTreeMap<String, Vec<ChannelTarget>>>,
}

impl Client {
    pub async fn new(config: ClientConfig) -> Result<Arc<Self>> {
        let uri: Uri = config.url.parse().context("failed to parse SEABIRD_URL")?;
        let mut channel_builder = Channel::builder(uri.clone());

        match uri.scheme_str() {
            None | Some("https") => {
                println!("Enabling tls");
                channel_builder = channel_builder
                    .tls_config(ClientTlsConfig::new().domain_name(uri.host().unwrap()));
            }
            _ => {}
        }

        let channel = channel_builder
            .connect()
            .await
            .context("Failed to connect to seabird")?;

        let auth_header: MetadataValue<Ascii> = format!("Bearer {}", config.token).parse()?;

        let seabird_client =
            SeabirdClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
                req.metadata_mut()
                    .insert("authorization", auth_header.clone());
                Ok(req)
            });

        Ok(Arc::new(Client {
            config,
            inner: Mutex::new(seabird_client),
            proxied_channels: RwLock::new(BTreeMap::new()),
        }))
    }

    pub async fn set_proxied_channels(
        &self,
        proxied_channels: BTreeMap<String, Vec<ChannelTarget>>,
    ) {
        let mut guard = self.proxied_channels.write().await;
        *guard = proxied_channels
    }

    pub async fn run(&self) -> Result<()> {
        let mut stream = {
            // We need to make sure the lock is dropped, so we can use the
            // client to make requests later.
            let mut inner = self.inner.lock().await;

            inner
                .stream_events(proto::StreamEventsRequest {
                    commands: HashMap::new(),
                })
                .await?
                .into_inner()
        };

        while let Some(event) = stream.next().await.transpose()? {
            info!("<-- {:?}", event);

            if let Some(inner) = event.inner {
                match self.handle_event(inner).await {
                    Err(err) => error!("failed to handle event: {}", err),
                    _ => {}
                }
            } else {
                warn!("Got SeabirdEvent missing an inner");
            }
        }

        Err(format_err!("run exited early"))
    }
}

impl Client {
    async fn handle_event(&self, event: SeabirdEvent) -> Result<()> {
        match event {
            SeabirdEvent::Action(action) => {
                info!("Action: {:?}", action);

                let source = action
                    .source
                    .ok_or_else(|| format_err!("event missing source"))?;
                let user = source
                    .user
                    .ok_or_else(|| format_err!("event missing user"))?;
                let text = action.text;

                self.send_msg(source.channel_id, |suffix| {
                    format!("* {}{} {}", user.display_name, suffix, text)
                })
                .await;
            }
            SeabirdEvent::Message(message) => {
                info!("Message: {:?}", message);

                let source = message
                    .source
                    .ok_or_else(|| format_err!("event missing source"))?;
                let user = source
                    .user
                    .ok_or_else(|| format_err!("event missing user"))?;
                let text = message.text;

                self.send_msg(source.channel_id, |suffix| {
                    format!("{}{}: {}", user.display_name, suffix, text)
                })
                .await;
            }
            SeabirdEvent::Command(command) => {
                info!("Command: {:?}", command);

                let source = command
                    .source
                    .ok_or_else(|| format_err!("event missing source"))?;
                let user = source
                    .user
                    .ok_or_else(|| format_err!("event missing user"))?;

                let cmd = command.command;
                let arg = command.arg;

                // TODO: maybe pull command prefix from some other API?
                if arg != "" {
                    self.send_msg(source.channel_id, |suffix| {
                        format!("{}{}: !{} {}", user.display_name, suffix, cmd, arg)
                    })
                    .await;
                } else {
                    self.send_msg(source.channel_id, |suffix| {
                        format!("{}{}: !{}", user.display_name, suffix, cmd)
                    })
                    .await;
                }
            }
            SeabirdEvent::Mention(mention) => {
                info!("Mention: {:?}", mention);

                let source = mention
                    .source
                    .ok_or_else(|| format_err!("event missing source"))?;
                let user = source
                    .user
                    .ok_or_else(|| format_err!("event missing user"))?;
                let text = mention.text;

                let nick = self.get_current_nick().await?;

                self.send_msg(source.channel_id, |suffix| {
                    format!("{}{}: {} {}", user.display_name, suffix, nick, text)
                })
                .await;
            }

            // Ignore all private message types as we can't proxy those.
            SeabirdEvent::PrivateMessage(_) | SeabirdEvent::PrivateAction(_) => {}
        }

        Ok(())
    }

    async fn get_current_nick(&self) -> Result<String> {
        Ok("seabird".to_string())
    }

    async fn send_msg<T>(&self, source: String, cb: T)
    where
        T: Fn(&str) -> String,
    {
        if let Some(channels) = self.proxied_channels.read().await.get(&source) {
            let mut inner = self.inner.lock().await;

            for channel in channels.iter() {
                let text = cb(channel.user_suffix.as_deref().unwrap_or(""));

                debug!("Proxying {} to {}", text, channel.id);

                if let Err(err) = inner
                    .send_message(proto::SendMessageRequest {
                        channel_id: channel.id.clone(),
                        text,
                    })
                    .await
                {
                    error!("Failed to send message to channel {}: {}", channel.id, err);
                } else {
                    debug!("Proxied message to {}", channel.id);
                }
            }
        }
    }
}
