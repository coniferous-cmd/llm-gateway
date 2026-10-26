package config

import (
	"fmt"
	"gopkg.in/yaml.v3"
	"strings"
)

const DefaultMaxBodyBytes = 8 << 20

// ConversationLog controls opt-in capture. Directory is config-relative until Load.
type ConversationLog struct {
	Enabled      bool
	Directory    string
	MaxBodyBytes int
}

func parseConversationLog(root *yaml.Node) (ConversationLog, error) {
	c := ConversationLog{Directory: "logs/conversations", MaxBodyBytes: DefaultMaxBodyBytes}
	n := field(root, "conversation_log")
	if n == root {
		return c, nil
	}
	if n.Kind != yaml.MappingNode {
		return c, fmt.Errorf("line %d: conversation_log must be a mapping", n.Line)
	}
	for i := 0; i+1 < len(n.Content); i += 2 {
		key, v := n.Content[i].Value, n.Content[i+1]
		fail := func() error { return fmt.Errorf("line %d: invalid conversation_log.%s", v.Line, key) }
		switch key {
		case "enabled":
			if v.Tag != "!!bool" || v.Decode(&c.Enabled) != nil {
				return c, fail()
			}
		case "directory":
			if v.Tag != "!!str" || strings.TrimSpace(v.Value) == "" {
				return c, fail()
			}
			c.Directory = v.Value
		case "max_body_bytes":
			if v.Tag != "!!int" || v.Decode(&c.MaxBodyBytes) != nil || c.MaxBodyBytes <= 0 {
				return c, fail()
			}
		}
	}
	return c, nil
}
