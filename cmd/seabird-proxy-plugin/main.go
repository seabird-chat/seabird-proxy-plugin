package main

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/signal"
	"syscall"

	"github.com/belak/x/slogx"
	"github.com/peterbourgon/ff/v4"
	"github.com/peterbourgon/ff/v4/ffhelp"

	proxy "github.com/seabird-chat/seabird-proxy-plugin"
)

func main() {
	fs := ff.NewFlagSet("seabird-proxy-plugin")

	// Flag names are chosen so ff's env var mapping lands on the names the
	// plugin has always used: --seabird-host becomes SEABIRD_HOST and so on.
	var (
		logFormat = defaultLogFormat()
		logLevel  = slogx.LevelInfo
	)
	fs.ValueLong("log-format", &logFormat, "log output format (json, pretty, or text)")
	fs.ValueLong("log-level", &logLevel, "log level (debug, info, warn, or error)")

	var (
		host       = fs.StringLong("seabird-host", "", "seabird-core URL")
		token      = fs.StringLong("seabird-token", "", "seabird-core auth token")
		configFile = fs.StringLong("proxy-config-file", "", "path to the proxied channel config")
		tag        = fs.StringLong("proxy-tag", "proxy", "sender name used for proxied messages")
	)

	cmd := &ff.Command{
		Name:  "seabird-proxy-plugin",
		Usage: "seabird-proxy-plugin [FLAGS]",
		Flags: fs,
		Exec: func(ctx context.Context, args []string) error {
			switch {
			case *host == "":
				return errors.New("--seabird-host is required")
			case *token == "":
				return errors.New("--seabird-token is required")
			case *configFile == "":
				return errors.New("--proxy-config-file is required")
			}

			logger := slogx.New(logFormat, logLevel)

			proxiedChannels, err := proxy.LoadConfig(*configFile)
			if err != nil {
				return err
			}

			client, err := proxy.NewSeabirdClient(*host, *token, *tag, logger)
			if err != nil {
				return fmt.Errorf("failed to dial seabird-core: %w", err)
			}
			defer client.Close() //nolint:errcheck

			logger.Info("dialed seabird-core", slogx.String("host", *host))

			client.SetProxiedChannels(proxiedChannels)

			go reloadOnHangup(ctx, client, *configFile, logger)

			return client.Run(ctx)
		},
	}

	if err := cmd.Parse(os.Args[1:], ff.WithEnvVars()); err != nil {
		if errors.Is(err, ff.ErrHelp) {
			fmt.Fprintf(os.Stdout, "%s\n", ffhelp.Command(cmd))
			return
		}

		fmt.Fprintf(os.Stderr, "%s\n", ffhelp.Command(cmd))
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()

	if err := cmd.Run(ctx); err != nil && !errors.Is(err, context.Canceled) {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}
}

// reloadOnHangup reloads the proxy config on SIGHUP. A config which fails to
// load is logged and ignored so the running config stays in place.
func reloadOnHangup(ctx context.Context, client *proxy.SeabirdClient, configFile string, logger *slogx.Logger) {
	hangup := make(chan os.Signal, 1)
	signal.Notify(hangup, syscall.SIGHUP)
	defer signal.Stop(hangup)

	for {
		select {
		case <-ctx.Done():
			return
		case <-hangup:
			logger.Info("got SIGHUP, reloading config")

			proxiedChannels, err := proxy.LoadConfig(configFile)
			if err != nil {
				logger.Warn("failed to reload config", slogx.Err(err))
				continue
			}

			client.SetProxiedChannels(proxiedChannels)

			logger.Info("reloaded config")
		}
	}
}

// defaultLogFormat is pretty on a terminal and JSON everywhere else, so
// production logs stay machine readable.
func defaultLogFormat() slogx.Format {
	if stat, err := os.Stdout.Stat(); err == nil && stat.Mode()&os.ModeCharDevice != 0 {
		return slogx.FormatPretty
	}

	return slogx.FormatJSON
}
