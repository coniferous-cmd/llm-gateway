// Package config loads the gateway's immutable startup configuration.
package config

import (
	"encoding/base64"
	"fmt"
	"net/url"
	"os"
	"path/filepath"
	"strings"

	"gopkg.in/yaml.v3"
)

const DefaultListen = "127.0.0.1:8080"

// DefaultE2EEListen is the encrypted peer port used when e2ee is enabled
// without an explicit listen address. Loopback by default: cross-machine
// deployments must set it explicitly (and firewall it).
const DefaultE2EEListen = "127.0.0.1:8081"

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

// RewriteRule replaces a literal escaped path prefix with Destination.
// An empty Destination represents the YAML '-' prefix-deletion marker.
// Rules are validated at startup and evaluated in configuration order.
type RewriteRule struct {
	Source      string
	Destination string
}

type Target struct {
	BaseURL string `yaml:"base_url"`
	APIKey  string `yaml:"api_key"`
	// Peer names an entry in E2EE.Peers; the request is forwarded over an
	// encrypted Noise session instead of plain HTTP. Mutually exclusive with
	// BaseURL.
	Peer string `yaml:"peer"`
	// Rewrite belongs to this target and is immutable after startup.
	Rewrite []RewriteRule `yaml:"-"`
}

// E2EE configures the gateway-to-gateway encrypted channel. Every gateway
// runs the same shape: its own listener and identity, plus the pinned public
// keys of the peers it is willing to talk to. There are no client/server
// roles; whoever dials initiates the handshake.
type E2EE struct {
	Listen      string `yaml:"listen"`
	IdentityKey string `yaml:"identity_key"`
	Peers       []Peer `yaml:"peers"`
}

// Peer is one trusted remote gateway.
type Peer struct {
	Name        string `yaml:"name"`
	BaseURL     string `yaml:"base_url"`
	IdentityPub string `yaml:"identity_pub"`
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
	// E2EE is nil when the gateway has no encrypted peer channel.
	E2EE *E2EE
	Keys map[string][]Entry
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
		E2EE   *struct {
			Listen      string `yaml:"listen"`
			IdentityKey string `yaml:"identity_key"`
			Peers       []struct {
				Name        string `yaml:"name"`
				BaseURL     string `yaml:"base_url"`
				IdentityPub string `yaml:"identity_pub"`
			} `yaml:"peers"`
		} `yaml:"e2ee"`
		Keys []struct {
			Token  string `yaml:"token"`
			Target struct {
				Target  `yaml:",inline"`
				Rewrite yaml.Node `yaml:"rewrite"`
			} `yaml:"target"`
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
	root := doc.Content[0]
	peerByName := map[string]Peer{}
	var e2eeNode *yaml.Node
	if raw.E2EE != nil {
		e2eeNode = field(root, "e2ee")
		cfg.E2EE = &E2EE{Listen: raw.E2EE.Listen, IdentityKey: raw.E2EE.IdentityKey}
		if cfg.E2EE.Listen == "" {
			cfg.E2EE.Listen = DefaultE2EEListen
		}
		if err := validateIdentityKey(field(e2eeNode, "identity_key"), raw.E2EE.IdentityKey); err != nil {
			return nil, err
		}
		peerNodes := field(e2eeNode, "peers")
		for j, p := range raw.E2EE.Peers {
			var node *yaml.Node
			if peerNodes.Kind == yaml.SequenceNode && j < len(peerNodes.Content) {
				node = peerNodes.Content[j]
			}
			line := peerLine(node, e2eeNode)
			if strings.TrimSpace(p.Name) == "" {
				return nil, fmt.Errorf("line %d: e2ee.peers[%d].name is required", line, j)
			}
			if _, dup := peerByName[p.Name]; dup {
				return nil, fmt.Errorf("line %d: e2ee.peers[%d].name %q is duplicated", line, j, p.Name)
			}
			base, err := parseOrigin(p.BaseURL)
			if err != nil {
				return nil, fmt.Errorf("line %d: e2ee.peers[%d].base_url must be an http(s) origin without credentials, path, query or fragment", line, j)
			}
			if err := validateIdentityPub(line, j, p.IdentityPub); err != nil {
				return nil, err
			}
			peer := Peer{Name: p.Name, BaseURL: base, IdentityPub: p.IdentityPub}
			cfg.E2EE.Peers = append(cfg.E2EE.Peers, peer)
			peerByName[p.Name] = peer
		}
	}
	entries := field(root, "keys")
	plainAddrs := map[string]int{} // host:port -> keys index (for conflict check)
	for i, entry := range raw.Keys {
		node := entries.Content[i]
		if strings.TrimSpace(entry.Token) == "" {
			return nil, fmt.Errorf("line %d: keys[%d].token is required", field(node, "token").Line, i)
		}
		targetNode := field(node, "target")
		hasBase := strings.TrimSpace(entry.Target.BaseURL) != ""
		hasPeer := strings.TrimSpace(entry.Target.Peer) != ""
		switch {
		case hasBase && hasPeer:
			return nil, fmt.Errorf("line %d: keys[%d].target must set exactly one of base_url or peer, not both", field(targetNode, "base_url").Line, i)
		case hasPeer:
			name := strings.TrimSpace(entry.Target.Peer)
			peer, ok := peerByName[name]
			if !ok {
				return nil, fmt.Errorf("line %d: keys[%d].target.peer %q is not declared in e2ee.peers", field(targetNode, "peer").Line, i, name)
			}
			// Resolve to the peer's e2ee address so the forwarding path can
			// treat BaseURL uniformly; Peer marks the target encrypted.
			entry.Target.BaseURL = peer.BaseURL
		case hasBase:
			base, err := parseOrigin(entry.Target.BaseURL)
			if err != nil {
				return nil, fmt.Errorf("line %d: keys[%d].target.base_url must be an http(s) origin without credentials, path, query or fragment", field(targetNode, "base_url").Line, i)
			}
			entry.Target.BaseURL = base
			if addr, err := originAddr(base); err == nil {
				plainAddrs[addr] = i
			}
		default:
			return nil, fmt.Errorf("line %d: keys[%d].target must set one of base_url or peer", targetNode.Line, i)
		}
		rules, err := parseRewrite(&entry.Target.Rewrite, i)
		if err != nil {
			return nil, err
		}
		entry.Target.Target.Rewrite = rules
		models, err := validateModels(field(node, "models"), entry.Token, cfg.Keys[entry.Token], entry.Models, i)
		if err != nil {
			return nil, err
		}
		cfg.Keys[entry.Token] = append(cfg.Keys[entry.Token], Entry{Target: entry.Target.Target, Models: models})
	}
	// A peer's e2ee address must never collide with a plaintext target: the
	// transport decides encryption by destination address alone, so an
	// overlap would send plaintext to a peer port or Noise to a provider.
	if cfg.E2EE != nil {
		for _, peer := range cfg.E2EE.Peers {
			addr, err := originAddr(peer.BaseURL)
			if err != nil {
				continue
			}
			if i, clash := plainAddrs[addr]; clash {
				return nil, fmt.Errorf("line %d: e2ee peer %q address %s collides with keys[%d].target.base_url", e2eeNode.Line, peer.Name, addr, i)
			}
		}
	}
	return cfg, nil
}

// parseOrigin validates a bare http(s) origin and returns it trimmed.
func parseOrigin(raw string) (string, error) {
	base := strings.TrimRight(strings.TrimSpace(raw), "/")
	u, err := url.Parse(base)
	if err != nil || u.Hostname() == "" || (u.Scheme != "http" && u.Scheme != "https") || u.User != nil || u.Path != "" || u.RawQuery != "" || u.ForceQuery || u.Fragment != "" || strings.Contains(base, "#") {
		return "", fmt.Errorf("not an http(s) origin")
	}
	return base, nil
}

// originAddr canonicalizes an origin to "host:port" for collision checks.
func originAddr(base string) (string, error) {
	u, err := url.Parse(base)
	if err != nil {
		return "", err
	}
	port := u.Port()
	if port == "" {
		if u.Scheme == "https" {
			port = "443"
		} else {
			port = "80"
		}
	}
	return u.Hostname() + ":" + port, nil
}

func validateIdentityKey(node *yaml.Node, encoded string) error {
	if strings.TrimSpace(encoded) == "" {
		return fmt.Errorf("line %d: e2ee.identity_key is required when e2ee is configured", node.Line)
	}
	raw, err := base64.StdEncoding.DecodeString(encoded)
	if err != nil || len(raw) != 32 {
		return fmt.Errorf("line %d: e2ee.identity_key must be a base64-encoded 32-byte X25519 private key", node.Line)
	}
	return nil
}

func validateIdentityPub(line, index int, encoded string) error {
	if strings.TrimSpace(encoded) == "" {
		return fmt.Errorf("line %d: e2ee.peers[%d].identity_pub is required", line, index)
	}
	raw, err := base64.StdEncoding.DecodeString(encoded)
	if err != nil || len(raw) != 32 {
		return fmt.Errorf("line %d: e2ee.peers[%d].identity_pub must be a base64-encoded 32-byte X25519 public key", line, index)
	}
	return nil
}

// peerLine reports the most precise line available for a peer entry.
func peerLine(node *yaml.Node, section *yaml.Node) int {
	if node != nil {
		return node.Line
	}
	return section.Line
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
