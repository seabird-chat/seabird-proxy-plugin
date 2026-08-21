# seabird-proxy-plugin

Seabird plugin which mirrors messages between channels, so a channel on one
backend can be bridged to a channel on another.

## Configuration

Every flag can also be set from the matching environment variable, so
`--seabird-host` and `SEABIRD_HOST` are interchangeable:

- `--seabird-host` / `--seabird-token` (required) - how to reach seabird-core.
- `--proxy-config-file` (required) - path to the channel config described below.
- `--proxy-tag` - the sender name used for proxied messages, defaulting to
  `proxy`. Messages this plugin sends are tagged with it so they don't get
  proxied back again.
- `--log-level` - `debug`, `info`, `warn` or `error`.
- `--log-format` - `pretty`, `json` or `text`, defaulting to pretty on a
  terminal and JSON everywhere else.

The config file lists each channel pair, along with optional decorations for
the original sender's display name:

```json
{
  "proxied_channels": [
    {
      "source": "irc://seabird/%23encoded%2Dtest",
      "target": "irc://seabird/%23encoded%2Dtest",
      "user_suffix": " (PROXY)"
    }
  ]
}
```

Proxying is one directional, so bridging two channels together needs an entry
in each direction. Send `SIGHUP` to reload the config without restarting.

Other plugins can opt an event out of proxying by setting the `proxy/skip` tag
to `1`.
