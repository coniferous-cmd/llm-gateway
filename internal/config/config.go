// Package config loads the gateway's immutable startup configuration.
package config

import (
	"fmt"
	"net/url"
	"os"
	"path/filepath"
	"strings"

	"gopkg.in/yaml.v3"
)

const DefaultListen = "127.0.0.1:8080"

// DefaultDir is the directory holding the config, pid and log files.
func DefaultDir() (string, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return "", fmt.Errorf("resolve home directory: %w", err)
	}
	return filepath.Join(home, ".config", "llm-gateway"), nil
}

// DefaultPath is the config location used when --config is not given.
func DefaultPath() (string, error) {
	dir, err := DefaultDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(dir, "config.yaml"), nil
}

type Target struct {
	BaseURL string `yaml:"base_url"`
	APIKey  string `yaml:"api_key"`
}

// Model maps the model name a caller uses (ID) to the name the provider
// receives (Name).
type Model struct {
	ID   string `yaml:"id"`
	Name string `yaml:"name"`
}

// Entry is one configured occurrence of a virtual key: its target and the
// models that target serves.
type Entry struct {
	Target Target
	Models []Model
}

type Config struct {
	Listen string
	Keys   map[string][]Entry
}

func Load(path string) (*Config, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read config: %w", err)
	}
	return Parse(data)
}

func Parse(data []byte) (*Config, error) {
	var doc yaml.Node
	if err := yaml.Unmarshal(data, &doc); err != nil {
		return nil, fmt.Errorf("parse config: %w", err)
	}
	if len(doc.Content) == 0 {
		return nil, fmt.Errorf("config: keys must not be empty")
	}
	var raw struct {
		Listen string `yaml:"listen"`
		Keys   []struct {
			Token  string  `yaml:"token"`
			Target Target  `yaml:"target"`
			Models []Model `yaml:"models"`
		} `yaml:"keys"`
	}
	if err := doc.Decode(&raw); err != nil {
		return nil, fmt.Errorf("decode config: %w", err)
	}
	if len(raw.Keys) == 0 {
		return nil, fmt.Errorf("config: keys must not be empty")
	}
	cfg := &Config{Listen: raw.Listen, Keys: make(map[string][]Entry)}
	if cfg.Listen == "" {
		cfg.Listen = DefaultListen
	}
	entries := field(doc.Content[0], "keys")
	for i, entry := range raw.Keys {
		node := entries.Content[i]
		if strings.TrimSpace(entry.Token) == "" {
			return nil, fmt.Errorf("line %d: keys[%d].token is required", field(node, "token").Line, i)
		}
		targetNode := field(node, "target")
		base := strings.TrimRight(entry.Target.BaseURL, "/")
		u, err := url.Parse(base)
		if err != nil || u.Hostname() == "" || (u.Scheme != "http" && u.Scheme != "https") || u.User != nil || u.Path != "" || u.RawQuery != "" || u.ForceQuery || u.Fragment != "" || strings.Contains(base, "#") {
			return nil, fmt.Errorf("line %d: keys[%d].target.base_url must be an http(s) origin without credentials, path, query or fragment", field(targetNode, "base_url").Line, i)
		}
		entry.Target.BaseURL = base
		models, err := validateModels(field(node, "models"), entry.Token, cfg.Keys[entry.Token], entry.Models, i)
		if err != nil {
			return nil, err
		}
		cfg.Keys[entry.Token] = append(cfg.Keys[entry.Token], Entry{Target: entry.Target, Models: models})
	}
	return cfg, nil
}

// validateModels checks one entry's models against the token's already accepted
// entries: id and name must be non-empty, and id must be unique per token.
func validateModels(node *yaml.Node, token string, prior []Entry, models []Model, keyIndex int) ([]Model, error) {
	seen := make(map[string]struct{})
	for _, entry := range prior {
		for _, model := range entry.Models {
			seen[model.ID] = struct{}{}
		}
	}
	for j, model := range models {
		item := node.Content[j]
		if strings.TrimSpace(model.ID) == "" {
			return nil, fmt.Errorf("line %d: keys[%d].models[%d].id is required", field(item, "id").Line, keyIndex, j)
		}
		if strings.TrimSpace(model.Name) == "" {
			return nil, fmt.Errorf("line %d: keys[%d].models[%d].name is required", field(item, "name").Line, keyIndex, j)
		}
		if _, dup := seen[model.ID]; dup {
			return nil, fmt.Errorf("line %d: keys[%d].models[%d].id %q is already declared for token %q", field(item, "id").Line, keyIndex, j, model.ID, token)
		}
		seen[model.ID] = struct{}{}
	}
	return models, nil
}

// Missing fields use the enclosing mapping's line for actionable errors.
func field(node *yaml.Node, name string) *yaml.Node {
	if node.Kind == yaml.AliasNode {
		node = node.Alias
	}
	for i := 0; i+1 < len(node.Content); i += 2 {
		if node.Content[i].Value == name {
			return node.Content[i+1]
		}
	}
	return node
}
