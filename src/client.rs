use std::collections::{BTreeMap, HashMap};

use tokio::sync::{Mutex, RwLock, mpsc};

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

// Formatting applied to the prefix + username + suffix of proxied messages.
// Multiple styles nest, with the first listed innermost.
#[derive(Clone, Copy, Debug, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserStyle {
    Bold,
    Italics,
    Underline,
    Strikethrough,
}

#[derive(Debug)]
pub struct ChannelTarget {
    id: String,
    user_prefix: Option<String>,
    user_suffix: Option<String>,
    user_style: Vec<UserStyle>,
}

impl ChannelTarget {
    pub fn new(
        id: String,
        user_prefix: Option<String>,
        user_suffix: Option<String>,
        user_style: Vec<UserStyle>,
    ) -> Self {
        ChannelTarget {
            id,
            user_prefix,
            user_suffix,
            user_style,
        }
    }

    // Builds the head block: lead, the styled label (prefix + name + suffix),
    // then tail. Returns the block and its plain text.
    fn head(&self, lead: &str, name: &str, tail: &str) -> (seabird::Block, String) {
        let label = format!(
            "{}{}{}",
            self.user_prefix.as_deref().unwrap_or(""),
            name,
            self.user_suffix.as_deref().unwrap_or("")
        );
        let plain = format!("{}{}{}", lead, label, tail);

        let styled =
            self.user_style
                .iter()
                .fold(
                    seabird::Block::new().text(label),
                    |inner, style| match style {
                        UserStyle::Bold => seabird::Block::new().bold(inner),
                        UserStyle::Italics => seabird::Block::new().italic(inner),
                        UserStyle::Underline => seabird::Block::new().underline(inner),
                        UserStyle::Strikethrough => seabird::Block::new().strikethrough(inner),
                    },
                );

        let mut head = seabird::Block::new();
        if !lead.is_empty() {
            head = head.text(lead);
        }
        head = head.append(styled);
        if !tail.is_empty() {
            head = head.text(tail);
        }

        (head, plain)
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
                    "* ",
                    &user.display_name,
                    " ",
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
                    "",
                    &user.display_name,
                    ": ",
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

                self.send_msg(
                    queue,
                    source.channel_id,
                    &tags,
                    "",
                    &user.display_name,
                    ": ",
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
                    "",
                    &user.display_name,
                    &format!(": {}: ", nick),
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

    // Sends a head followed by the body to each target. The head is built per
    // target so it can carry that target's prefix, suffix, and style.
    #[allow(clippy::too_many_arguments)]
    async fn send_msg(
        &self,
        queue: &mut mpsc::Sender<OutgoingMessage>,
        source: String,
        tags: &HashMap<String, String>,
        lead: &str,
        name: &str,
        tail: &str,
        body: String,
        block: Option<proto::Block>,
    ) -> Result<()> {
        if let Some(channels) = self.proxied_channels.read().await.get(&source) {
            for channel in channels.iter() {
                let (head, plain_head) = channel.head(lead, name, tail);
                let text = format!("{}{}", plain_head, body);

                // Events without a block (such as commands) are wrapped as text.
                let body_block = match block.clone() {
                    Some(block) => seabird::Block::from(block),
                    None => seabird::Block::new().text(body.clone()),
                };
                let root_block = Some(head.append(body_block).into());

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

#[cfg(test)]
mod tests {
    use super::*;

    fn target(style: Vec<UserStyle>) -> ChannelTarget {
        ChannelTarget::new(
            "irc://x/#c".to_string(),
            Some("<".to_string()),
            Some(">".to_string()),
            style,
        )
    }

    #[test]
    fn head_applies_style_to_label_only() {
        let (head, plain) = target(vec![UserStyle::Bold]).head("* ", "bob", " ");
        assert_eq!(plain, "* <bob> ");

        let block: proto::Block = head.into();
        let Some(proto::block::Inner::Container(c)) = block.inner else {
            panic!("expected container");
        };
        assert_eq!(c.inner.len(), 3);
        assert!(matches!(
            c.inner[0].inner,
            Some(proto::block::Inner::Text(_))
        ));
        match &c.inner[1].inner {
            Some(proto::block::Inner::Bold(b)) => match b.inner.as_ref().unwrap().inner {
                Some(proto::block::Inner::Text(ref t)) => assert_eq!(t.text, "<bob>"),
                ref other => panic!("expected text, got {:?}", other),
            },
            other => panic!("expected bold, got {:?}", other),
        }
        assert!(matches!(
            c.inner[2].inner,
            Some(proto::block::Inner::Text(_))
        ));
    }

    #[test]
    fn head_nests_multiple_styles() {
        let (head, _) = target(vec![UserStyle::Bold, UserStyle::Italics]).head("", "bob", "");
        let block: proto::Block = head.into();
        // Italics wraps bold, which wraps the text.
        let Some(proto::block::Inner::Italics(i)) = block.inner else {
            panic!("expected italics, got {:?}", block.inner);
        };
        let Some(proto::block::Inner::Bold(b)) = i.inner.unwrap().inner else {
            panic!("expected bold");
        };
        assert!(matches!(
            b.inner.unwrap().inner,
            Some(proto::block::Inner::Text(_))
        ));
    }

    #[test]
    fn head_without_style_is_text() {
        let (head, plain) = target(vec![]).head("", "bob", ": ");
        assert_eq!(plain, "<bob>: ");

        let block: proto::Block = head.into();
        let Some(proto::block::Inner::Container(c)) = block.inner else {
            panic!("expected container");
        };
        assert_eq!(c.inner.len(), 2);
        assert!(
            c.inner
                .iter()
                .all(|b| matches!(b.inner, Some(proto::block::Inner::Text(_))))
        );
    }
}
