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
    inner: Mutex<seabird::SeabirdClient>,
    proxied_channels: RwLock<BTreeMap<String, Vec<ChannelTarget>>>,
}

impl Client {
    pub async fn new(config: ClientConfig) -> Result<Arc<Self>> {
        let seabird_client = seabird::SeabirdClient::new(config.inner.clone()).await?;

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
                    let channel_id = action.channel_id.clone();
                    if let Err(err) = inner
                        .perform_action(
                            action.channel_id,
                            seabird::MessageContent::new(action.text, action.root_block),
                            Some(action.tags),
                        )
                        .await
                    {
                        warn!("failed to perform action on {}: {}", channel_id, err);
                    }
                }
                Some(OutgoingMessage::Message(message)) => {
                    let mut inner = self.inner.lock().await;
                    debug!("Sending message {} to {}", message.text, message.channel_id);
                    let channel_id = message.channel_id.clone();
                    if let Err(err) = inner
                        .send_message(
                            message.channel_id,
                            seabird::MessageContent::new(message.text, message.root_block),
                            Some(message.tags),
                        )
                        .await
                    {
                        warn!("failed to send message to {}: {}", channel_id, err);
                    }
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
                info!("Action: {:?}, Tags: {:?}", action, tags);

                let source = action
                    .source
                    .ok_or_else(|| format_err!("event missing source"))?;
                let user = source
                    .user
                    .ok_or_else(|| format_err!("event missing user"))?;

                self.send_msg(
                    queue,
                    source.channel_id,
                    &tags,
                    |prefix, suffix| format!("* {}{}{} ", prefix, user.display_name, suffix),
                    action.text,
                    action.root_block,
                )
                .await?;
            }
            SeabirdEventInner::Message(message) => {
                info!("Message: {:?}, Tags: {:?}", message, tags);

                let source = message
                    .source
                    .ok_or_else(|| format_err!("event missing source"))?;
                let user = source
                    .user
                    .ok_or_else(|| format_err!("event missing user"))?;

                self.send_msg(
                    queue,
                    source.channel_id,
                    &tags,
                    |prefix, suffix| format!("{}{}{}: ", prefix, user.display_name, suffix),
                    message.text,
                    message.root_block,
                )
                .await?;
            }
            SeabirdEventInner::Command(command) => {
                info!("Command: {:?}, Tags: {:?}", command, tags);

                let source = command
                    .source
                    .ok_or_else(|| format_err!("event missing source"))?;
                let user = source
                    .user
                    .ok_or_else(|| format_err!("event missing user"))?;

                let cmd = command.command;
                let arg = command.arg;

                // TODO: maybe pull command prefix from some other API?
                let body = if arg != "" {
                    format!("!{} {}", cmd, arg)
                } else {
                    format!("!{}", cmd)
                };

                // Commands have no block, so these are proxied as plain text.
                self.send_msg(
                    queue,
                    source.channel_id,
                    &tags,
                    |prefix, suffix| format!("{}{}{}: ", prefix, user.display_name, suffix),
                    body,
                    None,
                )
                .await?;
            }
            SeabirdEventInner::Mention(mention) => {
                info!("Mention: {:?}, Tags: {:?}", mention, tags);

                let source = mention
                    .source
                    .ok_or_else(|| format_err!("event missing source"))?;
                let user = source
                    .user
                    .ok_or_else(|| format_err!("event missing user"))?;

                let nick = self.get_current_nick().await?;

                self.send_msg(
                    queue,
                    source.channel_id,
                    &tags,
                    |prefix, suffix| {
                        format!("{}{}{}: {}: ", prefix, user.display_name, suffix, nick)
                    },
                    mention.text,
                    mention.root_block,
                )
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

                info!("Send Message: {:?}, Tags: {:?}", message, tags);

                self.send_raw_msg(
                    queue,
                    message.channel_id,
                    &tags,
                    message.text,
                    message.root_block,
                )
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

                info!("Perform Action: {:?}, Tags: {:?}", action, tags);

                self.perform_raw_action(
                    queue,
                    action.channel_id,
                    &tags,
                    action.text,
                    action.root_block,
                )
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

    // Sends head + body (and head + block, if there is one) to each target.
    // The head is built per target from its user prefix and suffix.
    async fn send_msg<T>(
        &self,
        queue: &mut mpsc::Sender<OutgoingMessage>,
        source: String,
        tags: &HashMap<String, String>,
        head: T,
        body: String,
        block: Option<proto::Block>,
    ) -> Result<()>
    where
        T: Fn(&str, &str) -> String,
    {
        if let Some(channels) = self.proxied_channels.read().await.get(&source) {
            for channel in channels.iter() {
                let head = head(
                    channel.user_prefix.as_deref().unwrap_or(""),
                    channel.user_suffix.as_deref().unwrap_or(""),
                );
                let text = format!("{}{}", head, body);
                let root_block = block.clone().map(|b| prepend_text(head, b));

                debug!("Queuing message {} to {}", text, channel.id);

                queue
                    .send(OutgoingMessage::Message(proto::SendMessageRequest {
                        channel_id: channel.id.clone(),
                        text,
                        root_block,
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
        root_block: Option<proto::Block>,
    ) -> Result<()> {
        if let Some(channels) = self.proxied_channels.read().await.get(&source) {
            for channel in channels.iter() {
                debug!("Queuing message {} to {}", text, channel.id);

                queue
                    .send(OutgoingMessage::Message(proto::SendMessageRequest {
                        channel_id: channel.id.clone(),
                        text: text.clone(),
                        root_block: root_block.clone(),
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
        root_block: Option<proto::Block>,
    ) -> Result<()> {
        if let Some(channels) = self.proxied_channels.read().await.get(&source) {
            for channel in channels.iter() {
                debug!("Queuing action {} on {}", text, channel.id);

                queue
                    .send(OutgoingMessage::Action(proto::PerformActionRequest {
                        channel_id: channel.id.clone(),
                        text: text.clone(),
                        root_block: root_block.clone(),
                        tags: tags.clone(),
                    }))
                    .await?;
            }
        }

        Ok(())
    }
}

// Wraps block in a container preceded by a text block containing head. Core
// recomputes the plain fields, but we fill them in anyway.
fn prepend_text(head: String, block: proto::Block) -> proto::Block {
    let plain = format!("{}{}", head, block.plain);
    let head_block = proto::Block {
        plain: head.clone(),
        inner: Some(proto::block::Inner::Text(proto::TextBlock { text: head })),
    };

    proto::Block {
        plain,
        inner: Some(proto::block::Inner::Container(proto::ContainerBlock {
            inner: vec![head_block, block],
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepend_text_wraps_block() {
        let bold = proto::Block {
            plain: "hi".to_string(),
            inner: Some(proto::block::Inner::Bold(Box::new(proto::BoldBlock {
                inner: Some(Box::new(proto::Block {
                    plain: "hi".to_string(),
                    inner: Some(proto::block::Inner::Text(proto::TextBlock {
                        text: "hi".to_string(),
                    })),
                })),
            }))),
        };

        let out = prepend_text("bob: ".to_string(), bold.clone());
        assert_eq!(out.plain, "bob: hi");
        match out.inner {
            Some(proto::block::Inner::Container(c)) => {
                assert_eq!(c.inner.len(), 2);
                assert_eq!(c.inner[0].plain, "bob: ");
                assert_eq!(c.inner[1], bold);
            }
            other => panic!("expected container, got {:?}", other),
        }
    }
}
