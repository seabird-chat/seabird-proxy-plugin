package proxy

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/alecthomas/assert/v2"
)

func TestLoadConfig(t *testing.T) {
	t.Parallel()

	filename := filepath.Join(t.TempDir(), "config.json")
	err := os.WriteFile(filename, []byte(`{
		"proxied_channels": [
			{"source": "a", "target": "b", "user_suffix": " (PROXY)"},
			{"source": "a", "target": "c", "user_prefix": "<"},
			{"source": "b", "target": "a"}
		]
	}`), 0o600)
	assert.NoError(t, err)

	config, err := LoadConfig(filename)
	assert.NoError(t, err)

	assert.Equal(t, map[string][]ChannelTarget{
		"a": {
			{ID: "b", UserSuffix: " (PROXY)"},
			{ID: "c", UserPrefix: "<"},
		},
		"b": {
			{ID: "a"},
		},
	}, config)
}

func TestLoadConfigErrors(t *testing.T) {
	t.Parallel()

	_, err := LoadConfig(filepath.Join(t.TempDir(), "missing.json"))
	assert.Error(t, err)

	filename := filepath.Join(t.TempDir(), "invalid.json")
	assert.NoError(t, os.WriteFile(filename, []byte("nope"), 0o600))

	_, err = LoadConfig(filename)
	assert.Error(t, err)
}
