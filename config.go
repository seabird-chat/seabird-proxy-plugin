package proxy

import (
	"encoding/json"
	"fmt"
	"os"
)

// ChannelTarget is a channel messages get proxied to, along with the decorations
// to apply to the original sender's display name.
type ChannelTarget struct {
	ID         string
	UserPrefix string
	UserSuffix string
}

type configFile struct {
	ProxiedChannels []struct {
		Source     string `json:"source"`
		Target     string `json:"target"`
		UserPrefix string `json:"user_prefix"`
		UserSuffix string `json:"user_suffix"`
	} `json:"proxied_channels"`
}

// LoadConfig reads a config file and groups the proxy targets by source channel.
func LoadConfig(filename string) (map[string][]ChannelTarget, error) {
	data, err := os.ReadFile(filename)
	if err != nil {
		return nil, fmt.Errorf("failed to read config: %w", err)
	}

	var config configFile
	if err := json.Unmarshal(data, &config); err != nil {
		return nil, fmt.Errorf("failed to parse config: %w", err)
	}

	out := make(map[string][]ChannelTarget)
	for _, channel := range config.ProxiedChannels {
		out[channel.Source] = append(out[channel.Source], ChannelTarget{
			ID:         channel.Target,
			UserPrefix: channel.UserPrefix,
			UserSuffix: channel.UserSuffix,
		})
	}

	return out, nil
}
