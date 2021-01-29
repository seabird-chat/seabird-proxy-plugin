use std::collections::{BTreeMap, HashMap};

use tokio::sync::{mpsc, Mutex, RwLock};

use crate::prelude::*;

#[derive(Debug)]
pub struct ClientConfig {
    pub inner: seabird::ClientConfig,
    pub tag: String,
}

impl ClientConfig {
    pub fn new(url: String, token: String, tag: String) -> Self {
        ClientConfig {
            inner: seabird::ClientConfig { url, token },
            tag,
        }
    }
}

#[derive(Debug)]
enum OutgoingMessage {
    Action(proto::PerformActionRequest),
    Message(proto::SendMessageRequest),
}

#[derive(Debug)]
pub struct ChannelTarget {
    id: String,
    user_prefix: Option<String>,
    user_suffix: Option<String>,
}

impl ChannelTarget {
    pub fn new(id: String, user_prefix: Option<String>, user_suffix: Option<String>) -> Self {
        ChannelTarget {
            id,
            user_prefix,
            user_suffix,
        }
    }
}

// Client represents the running proxy
#[derive(Debug)]
pub struct Client {
    config: ClientConfig,
    inner: Mutex<seabird::Client>,
    proxied_channels: RwLock<BTreeMap<String, Vec<ChannelTarget>>>,
}

impl Client {
    pub async fn new(config: ClientConfig) -> Result<Arc<Self>> {
        let seabird_client = seabird::Client::new(config.inner.clone()).await?;

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
        // We want a fairly large queue because these messages are small and
        // sometimes we'll be proxying to multiple channels.
        let (writer, reader) = mpsc::channel(100);
        futures::future::try_join(self.run_reader(writer), self.run_writer(reader)).await?;

        Err(format_err!("run exited early"))
    }
}

impl Client {
    async fn run_reader(&self, mut queue: mpsc::Sender<OutgoingMessage>) -> Result<()> {
        debug!("Getting stream");

        let mut stream = self
            .inner
            .lock()
            .await
            .inner_mut_ref()
            .stream_events(proto::StreamEventsRequest {
                commands: HashMap::new(),
            })
            .await?
            .into_inner();

        debug!("Got stream");

        while let Some(event) = stream.next().await.transpose()? {
            info!("<-- {:?}", event);

            match self.handle_event(&mut queue, event).await {
                Err(err) => error!("failed to handle event: {}", err),
                _ => {}
            }
        }

        Err(format_err!("run_reader exited early"))
    }

    async fn run_writer(&self, mut queue: mpsc::Receiver<OutgoingMessage>) -> Result<()> {
        loop {
            match queue.recv().await {
                Some(OutgoingMessage::Action(action)) => {
                    let mut inner = self.inner.lock().await;
                    debug!("Performing action {} on {}", action.text, action.channel_id);
                    inner
                        .perform_action(action.channel_id, action.text, None)
                        .await?;
                }
                Some(OutgoingMessage::Message(message)) => {
                    let mut inner = self.inner.lock().await;
                    debug!("Sending message {} to {}", message.text, message.channel_id);
                    inner
                        .send_message(message.channel_id, message.text, None)
                        .await?;
                }
                None => return Err(format_err!("run_writer exited early")),
            }
        }
    }

    async fn handle_event(
        &self,
        queue: &mut mpsc::Sender<OutgoingMessage>,
        event: SeabirdEvent,
    ) -> Result<()> {
        // If the plugin requested for this event to not be proxied, we need to
        // skip it.
        if event
            .tags
            .get("proxy/skip")
            .map(String::as_str)
            .unwrap_or("0")
            == "1"
        {
            return Ok(());
        }

        let tags = event.tags;

        let inner = event
            .inner
            .ok_or_else(|| format_err!("SeabirdEvent missing an inner"))?;

        match inner {
            SeabirdEventInner::Action(action) => {
                info!("Action: {:?}", action);

                let source = action
                    .source
                    .ok_or_else(|| format_err!("event missing source"))?;
                let user = source
                    .user
                    .ok_or_else(|| format_err!("event missing user"))?;
                let text = action.text;

                self.send_msg(queue, source.channel_id, &tags, |prefix, suffix| {
                    format!("* {}{}{} {}", prefix, user.display_name, suffix, text)
                })
                .await?;
            }
            SeabirdEventInner::Message(message) => {
                info!("Message: {:?}", message);

                let source = message
                    .source
                    .ok_or_else(|| format_err!("event missing source"))?;
                let user = source
                    .user
                    .ok_or_else(|| format_err!("event missing user"))?;
                let text = message.text;

                self.send_msg(queue, source.channel_id, &tags, |prefix, suffix| {
                    format!("{}{}{}: {}", prefix, user.display_name, suffix, text)
                })
                .await?;
            }
            SeabirdEventInner::Command(command) => {
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
                    self.send_msg(queue, source.channel_id, &tags, |prefix, suffix| {
                        format!(
                            "{}{}{}: !{} {}",
                            prefix, user.display_name, suffix, cmd, arg
                        )
                    })
                    .await?;
                } else {
                    self.send_msg(queue, source.channel_id, &tags, |prefix, suffix| {
                        format!("{}{}{}: !{}", prefix, user.display_name, suffix, cmd)
                    })
                    .await?;
                }
            }
            SeabirdEventInner::Mention(mention) => {
                info!("Mention: {:?}", mention);

                let source = mention
                    .source
                    .ok_or_else(|| format_err!("event missing source"))?;
                let user = source
                    .user
                    .ok_or_else(|| format_err!("event missing user"))?;
                let text = mention.text;

                let nick = self.get_current_nick().await?;

                self.send_msg(queue, source.channel_id, &tags, |prefix, suffix| {
                    format!(
                        "{}{}{}: {}: {}",
                        prefix, user.display_name, suffix, nick, text
                    )
                })
                .await?;
            }

            // Seabird-sent events
            SeabirdEventInner::SendMessage(message) => {
                if message.sender == self.config.tag {
                    debug!(
                        "Skipping Send Message from {}: {:?}",
                        message.sender, message
                    );
                    return Ok(());
                }

                info!("Send Message: {:?}", message);

                self.send_raw_msg(queue, message.channel_id, &tags, message.text)
                    .await?;
            }
            SeabirdEventInner::PerformAction(action) => {
                if action.sender == self.config.tag {
                    debug!(
                        "Skipping Perform Action from {}: {:?}",
                        action.sender, action
                    );
                    return Ok(());
                }

                info!("Perform Action: {:?}", action);

                self.perform_raw_action(queue, action.channel_id, &tags, action.text)
                    .await?;
            }

            // Ignore all private message types as we can't proxy those.
            SeabirdEventInner::PrivateMessage(_)
            | SeabirdEventInner::PrivateAction(_)
            | SeabirdEventInner::SendPrivateMessage(_)
            | SeabirdEventInner::PerformPrivateAction(_) => {}
        }

        Ok(())
    }

    // TODO: make this better - this should probably be actually implemented
    async fn get_current_nick(&self) -> Result<String> {
        Ok("seabird".to_string())
    }

    async fn send_msg<T>(
        &self,
        queue: &mut mpsc::Sender<OutgoingMessage>,
        source: String,
        tags: &HashMap<String, String>,
        cb: T,
    ) -> Result<()>
    where
        T: Fn(&str, &str) -> String,
    {
        if let Some(channels) = self.proxied_channels.read().await.get(&source) {
            for channel in channels.iter() {
                let text = cb(
                    channel.user_prefix.as_deref().unwrap_or(""),
                    channel.user_suffix.as_deref().unwrap_or(""),
                );

                debug!("Queuing message {} to {}", text, channel.id);

                queue
                    .send(OutgoingMessage::Message(proto::SendMessageRequest {
                        channel_id: channel.id.clone(),
                        text,
                        tags: tags.clone(),
                    }))
                    .await?;
            }
        }

        Ok(())
    }

    async fn send_raw_msg(
        &self,
        queue: &mut mpsc::Sender<OutgoingMessage>,
        source: String,
        tags: &HashMap<String, String>,
        text: String,
    ) -> Result<()> {
        if let Some(channels) = self.proxied_channels.read().await.get(&source) {
            for channel in channels.iter() {
                debug!("Queuing message {} to {}", text, channel.id);

                queue
                    .send(OutgoingMessage::Message(proto::SendMessageRequest {
                        channel_id: channel.id.clone(),
                        text: text.clone(),
                        tags: tags.clone(),
                    }))
                    .await?;
            }
        }

        Ok(())
    }

    async fn perform_raw_action(
        &self,
        queue: &mut mpsc::Sender<OutgoingMessage>,
        source: String,
        tags: &HashMap<String, String>,
        text: String,
    ) -> Result<()> {
        if let Some(channels) = self.proxied_channels.read().await.get(&source) {
            for channel in channels.iter() {
                debug!("Queuing action {} on {}", text, channel.id);

                queue
                    .send(OutgoingMessage::Action(proto::PerformActionRequest {
                        channel_id: channel.id.clone(),
                        text: text.clone(),
                        tags: tags.clone(),
                    }))
                    .await?;
            }
        }

        Ok(())
    }
}
