// Package config loads the gateway's immutable startup configuration.
package config

import (
	"fmt"
	"net/url"
	"os"
	"strings"

	"gopkg.in/yaml.v3"
)

const DefaultListen = "127.0.0.1:8080"

type Target struct {
	BaseURL string `yaml:"base_url"`
	APIKey  string `yaml:"api_key"`
}

type Config struct {
	Listen string
	Keys   map[string][]Target
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
			Token  string `yaml:"token"`
			Target Target `yaml:"target"`
		} `yaml:"keys"`
	}
	if err := doc.Decode(&raw); err != nil {
		return nil, fmt.Errorf("decode config: %w", err)
	}
	if len(raw.Keys) == 0 {
		return nil, fmt.Errorf("config: keys must not be empty")
	}
	cfg := &Config{Listen: raw.Listen, Keys: make(map[string][]Target)}
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
		cfg.Keys[entry.Token] = append(cfg.Keys[entry.Token], entry.Target)
	}
	return cfg, nil
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
