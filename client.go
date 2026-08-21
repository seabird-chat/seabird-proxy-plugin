package proxy

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"sync"
	"time"

	"github.com/belak/x/slogx"
	"github.com/seabird-chat/seabird-go"
	"github.com/seabird-chat/seabird-go/pb"
)

const (
	// outgoingQueueSize is how many messages can be waiting to be sent. These
	// messages are small and a single event can be proxied to multiple
	// channels, so the queue is fairly large.
	outgoingQueueSize = 100

	// requestTimeout bounds how long a single send is allowed to take.
	requestTimeout = 5 * time.Second

	// skipTag lets other plugins mark an event as not to be proxied.
	skipTag = "proxy/skip"
)

// outgoing is a message or action waiting to be sent to a target channel.
type outgoing struct {
	action    bool
	channelID string
	text      string
	tags      map[string]string
}

// SeabirdClient proxies messages between channels.
type SeabirdClient struct {
	*seabird.Client

	logger *slog.Logger
	tag    string

	mu              sync.RWMutex
	proxiedChannels map[string][]ChannelTarget
}

// NewSeabirdClient returns a new seabird client. The tag is used both to mark
// messages this plugin sends and to avoid proxying them back again.
func NewSeabirdClient(seabirdCoreURL, seabirdCoreToken, tag string, logger *slog.Logger) (*SeabirdClient, error) {
	seabirdClient, err := seabird.NewClient(seabirdCoreURL, seabirdCoreToken)
	if err != nil {
		return nil, err
	}

	return &SeabirdClient{
		Client: seabirdClient,
		logger: logger,
		tag:    tag,
	}, nil
}

// SetProxiedChannels replaces the current proxy targets. It is safe to call
// while the client is running, which is how config reloads are handled.
func (c *SeabirdClient) SetProxiedChannels(proxiedChannels map[string][]ChannelTarget) {
	c.mu.Lock()
	defer c.mu.Unlock()

	c.proxiedChannels = proxiedChannels
}

func (c *SeabirdClient) targets(source string) []ChannelTarget {
	c.mu.RLock()
	defer c.mu.RUnlock()

	return c.proxiedChannels[source]
}

// Run streams events until the context is cancelled or the stream fails.
func (c *SeabirdClient) Run(ctx context.Context) error {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()

	// Sends happen on their own goroutine so a slow core can't stall event
	// handling, and so proxied messages keep their relative order.
	queue := make(chan outgoing, outgoingQueueSize)

	errs := make(chan error, 2)

	go func() { errs <- c.runReader(ctx, queue) }()
	go func() { errs <- c.runWriter(ctx, queue) }()

	return <-errs
}

func (c *SeabirdClient) runReader(ctx context.Context, queue chan<- outgoing) error {
	stream, err := c.StreamEvents(nil)
	if err != nil {
		return fmt.Errorf("failed to open event stream: %w", err)
	}
	defer stream.Close() //nolint:errcheck

	c.logger.Info("event stream open")

	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case event, ok := <-stream.C:
			if !ok {
				if err := stream.Close(); err != nil {
					return fmt.Errorf("event stream closed: %w", err)
				}

				return errors.New("event stream closed without an error")
			}

			if err := c.handleEvent(ctx, queue, event); err != nil {
				c.logger.Error("failed to handle event", slogx.Err(err))
			}
		}
	}
}

func (c *SeabirdClient) runWriter(ctx context.Context, queue <-chan outgoing) error {
	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case msg := <-queue:
			if err := c.send(ctx, msg); err != nil {
				c.logger.Error("failed to send message",
					slogx.String("channel_id", msg.channelID),
					slogx.Err(err))
			}
		}
	}
}

func (c *SeabirdClient) send(ctx context.Context, msg outgoing) error {
	ctx, cancel := context.WithTimeout(ctx, requestTimeout)
	defer cancel()

	if msg.action {
		c.logger.Debug("performing action",
			slogx.String("channel_id", msg.channelID),
			slogx.String("text", msg.text))

		_, err := c.Inner.PerformAction(ctx, &pb.PerformActionRequest{
			ChannelId: msg.channelID,
			Text:      msg.text,
			Tags:      msg.tags,
		})

		return err
	}

	c.logger.Debug("sending message",
		slogx.String("channel_id", msg.channelID),
		slogx.String("text", msg.text))

	_, err := c.Inner.SendMessage(ctx, &pb.SendMessageRequest{
		ChannelId: msg.channelID,
		Text:      msg.text,
		Tags:      msg.tags,
	})

	return err
}

func (c *SeabirdClient) handleEvent(ctx context.Context, queue chan<- outgoing, event *pb.Event) error {
	c.logger.Debug("received event", slogx.Any("event", event))

	tags := event.GetTags()

	// If a plugin asked for this event not to be proxied, skip it.
	if tags[skipTag] == "1" {
		return nil
	}

	switch inner := event.GetInner().(type) {
	case *pb.Event_Action:
		action := inner.Action

		source, user, err := sourceAndUser(action.GetSource())
		if err != nil {
			return err
		}

		return c.queueMessages(ctx, queue, source, tags, func(prefix, suffix string) string {
			return fmt.Sprintf("* %s%s%s %s", prefix, user, suffix, action.GetText())
		})

	case *pb.Event_Message:
		message := inner.Message

		source, user, err := sourceAndUser(message.GetSource())
		if err != nil {
			return err
		}

		return c.queueMessages(ctx, queue, source, tags, func(prefix, suffix string) string {
			return fmt.Sprintf("%s%s%s: %s", prefix, user, suffix, message.GetText())
		})

	case *pb.Event_Command:
		command := inner.Command

		source, user, err := sourceAndUser(command.GetSource())
		if err != nil {
			return err
		}

		// TODO: maybe pull the command prefix from some other API?
		text := "!" + command.GetCommand()
		if arg := command.GetArg(); arg != "" {
			text += " " + arg
		}

		return c.queueMessages(ctx, queue, source, tags, func(prefix, suffix string) string {
			return fmt.Sprintf("%s%s%s: %s", prefix, user, suffix, text)
		})

	case *pb.Event_Mention:
		mention := inner.Mention

		source, user, err := sourceAndUser(mention.GetSource())
		if err != nil {
			return err
		}

		return c.queueMessages(ctx, queue, source, tags, func(prefix, suffix string) string {
			return fmt.Sprintf("%s%s%s: %s: %s", prefix, user, suffix, c.currentNick(), mention.GetText())
		})

	// Events sent by other plugins. These are already formatted, so they get
	// proxied as-is.
	case *pb.Event_SendMessage:
		message := inner.SendMessage
		if message.GetSender() == c.tag {
			return nil
		}

		return c.queueRaw(ctx, queue, message.GetChannelId(), tags, message.GetText(), false)

	case *pb.Event_PerformAction:
		action := inner.PerformAction
		if action.GetSender() == c.tag {
			return nil
		}

		return c.queueRaw(ctx, queue, action.GetChannelId(), tags, action.GetText(), true)

	case nil:
		return errors.New("event missing an inner event")
	}

	// Everything else is a private message type, which can't be proxied.
	return nil
}

// queueMessages renders the message once per target so each target can apply
// its own prefix and suffix to the sender's display name.
func (c *SeabirdClient) queueMessages(
	ctx context.Context,
	queue chan<- outgoing,
	source string,
	tags map[string]string,
	render func(prefix, suffix string) string,
) error {
	for _, target := range c.targets(source) {
		if err := c.queue(ctx, queue, outgoing{
			channelID: target.ID,
			text:      render(target.UserPrefix, target.UserSuffix),
			tags:      tags,
		}); err != nil {
			return err
		}
	}

	return nil
}

func (c *SeabirdClient) queueRaw(
	ctx context.Context,
	queue chan<- outgoing,
	source string,
	tags map[string]string,
	text string,
	action bool,
) error {
	for _, target := range c.targets(source) {
		if err := c.queue(ctx, queue, outgoing{
			action:    action,
			channelID: target.ID,
			text:      text,
			tags:      tags,
		}); err != nil {
			return err
		}
	}

	return nil
}

func (c *SeabirdClient) queue(ctx context.Context, queue chan<- outgoing, msg outgoing) error {
	select {
	case queue <- msg:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

// TODO: make this better - this should probably be actually implemented
func (c *SeabirdClient) currentNick() string {
	return "seabird"
}

func sourceAndUser(source *pb.ChannelSource) (string, string, error) {
	if source == nil {
		return "", "", errors.New("event missing source")
	}

	if source.GetUser() == nil {
		return "", "", errors.New("event missing user")
	}

	return source.GetChannelId(), source.GetUser().GetDisplayName(), nil
}
