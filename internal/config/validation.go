package config

import (
	"fmt"
	"net"
	"regexp"
	"strconv"
	"strings"
	"unicode"

	"gopkg.in/yaml.v3"
)

// A nil child describes a scalar; a sequence uses its element schema.
type fieldSchema map[string]fieldSchema

var configFields = fieldSchema{
	"listen":           nil,
	"conversation_log": {"enabled": nil, "directory": nil, "max_body_bytes": nil},
	"e2ee":             {"listen": nil, "identity_key": nil, "peers": {"name": nil, "base_url": nil, "identity_pub": nil}},
	"keys":             {"token": nil, "target": {"base_url": nil, "api_key": nil, "peer": nil, "expire": nil, "rewrite": nil}, "models": {"id": nil, "name": nil}},
}

// Expand aliases and merges before semantic parsing so custom node parsers see
// the same effective mappings as yaml.Decode. Explicit fields win; the first
// mapping in a merge sequence wins over subsequent mappings.
func expandConfigNode(n *yaml.Node, active map[*yaml.Node]bool) (*yaml.Node, error) {
	if active[n] {
		return nil, fmt.Errorf("line %d: cyclic YAML alias", n.Line)
	}
	active[n] = true
	defer delete(active, n)
	if n.Kind == yaml.AliasNode {
		return expandConfigNode(n.Alias, active)
	}
	out := *n
	out.Content = nil
	for _, child := range n.Content {
		c, err := expandConfigNode(child, active)
		if err != nil {
			return nil, err
		}
		out.Content = append(out.Content, c)
	}
	if n.Kind != yaml.MappingNode {
		return &out, nil
	}
	explicit := map[string]bool{}
	for i := 0; i+1 < len(out.Content); i += 2 {
		if out.Content[i].Tag != "!!merge" {
			explicit[out.Content[i].Value] = true
		}
	}
	var fields []*yaml.Node
	var merge func(*yaml.Node) error
	merge = func(m *yaml.Node) error {
		if m.Kind == yaml.SequenceNode {
			for _, item := range m.Content {
				if err := merge(item); err != nil {
					return err
				}
			}
			return nil
		}
		if m.Kind != yaml.MappingNode {
			return fmt.Errorf("line %d: YAML merge must reference a mapping", m.Line)
		}
		for i := 0; i+1 < len(m.Content); i += 2 {
			if !explicit[m.Content[i].Value] {
				explicit[m.Content[i].Value] = true
				fields = append(fields, m.Content[i], m.Content[i+1])
			}
		}
		return nil
	}
	for i := 0; i+1 < len(out.Content); i += 2 {
		k, v := out.Content[i], out.Content[i+1]
		if k.Tag == "!!merge" {
			if err := merge(v); err != nil {
				return nil, err
			}
		} else {
			fields = append(fields, k, v)
		}
	}
	out.Content = fields
	return &out, nil
}

func validateFields(n *yaml.Node, schema fieldSchema) error {
	if schema == nil {
		return nil
	}
	if n.Kind == yaml.DocumentNode || n.Kind == yaml.SequenceNode {
		for _, item := range n.Content {
			if err := validateFields(item, schema); err != nil {
				return err
			}
		}
	}
	if n.Kind == yaml.MappingNode {
		for i := 0; i+1 < len(n.Content); i += 2 {
			k, v := n.Content[i], n.Content[i+1]
			child, ok := schema[k.Value]
			if !ok {
				return fmt.Errorf("line %d: unknown configuration field %q", k.Line, k.Value)
			}
			if err := validateFields(v, child); err != nil {
				return err
			}
		}
	}
	return nil
}

// yaml.TypeError can include the offending scalar, which might be a secret.
var yamlValueError = regexp.MustCompile(`cannot unmarshal .* into`)

func safeDecodeError(err error) error {
	if e, ok := err.(*yaml.TypeError); ok {
		messages := make([]string, len(e.Errors))
		for i, message := range e.Errors {
			messages[i] = yamlValueError.ReplaceAllString(message, "cannot decode value into")
		}
		return fmt.Errorf("%s", strings.Join(messages, "; "))
	}
	return err
}

func validateListen(n *yaml.Node, name, address string) error {
	fail := func() error {
		return fmt.Errorf("line %d: %s must be a host:numeric-port address with port 0-65535", n.Line, name)
	}
	if strings.ContainsFunc(address, func(r rune) bool { return unicode.IsSpace(r) || unicode.IsControl(r) }) {
		return fail()
	}
	host, port, err := net.SplitHostPort(address)
	if err != nil || port == "" {
		return fail()
	}
	for _, r := range port {
		if r < '0' || r > '9' {
			return fail()
		}
	}
	p, err := strconv.ParseUint(port, 10, 16)
	if err != nil || p > 65535 {
		return fail()
	}
	if strings.HasPrefix(address, "[") || strings.Contains(host, ":") {
		ip := host
		if i := strings.LastIndexByte(ip, '%'); i >= 0 {
			if i == len(ip)-1 {
				return fail()
			}
			ip = ip[:i]
		}
		if net.ParseIP(ip) == nil || !strings.Contains(ip, ":") {
			return fail()
		}
	} else if host != "" && net.ParseIP(host) == nil {
		hostname := strings.TrimSuffix(host, ".")
		if len(hostname) > 253 {
			return fail()
		}
		for _, label := range strings.Split(hostname, ".") {
			if len(label) == 0 || len(label) > 63 || label[0] == '-' || label[len(label)-1] == '-' {
				return fail()
			}
			for _, r := range label {
				if !(r >= 'a' && r <= 'z' || r >= 'A' && r <= 'Z' || r >= '0' && r <= '9' || r == '-') {
					return fail()
				}
			}
		}
	}
	return nil
}
