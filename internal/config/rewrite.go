package config

import (
	"fmt"
	"net/url"
	"strings"
	"unicode"

	"gopkg.in/yaml.v3"
)

func parseRewrite(node *yaml.Node, keyIndex int) ([]RewriteRule, error) {
	if node.Kind == 0 { // Omitted, unlike an explicitly supplied null.
		return nil, nil
	}
	for node.Kind == yaml.AliasNode {
		node = node.Alias
	}
	if node.Kind != yaml.SequenceNode {
		return nil, fmt.Errorf("line %d: keys[%d].target.rewrite must be a list of strings", node.Line, keyIndex)
	}
	var rules []RewriteRule
	for i, item := range node.Content {
		line := item.Line
		for item.Kind == yaml.AliasNode {
			item = item.Alias
		}
		if item.Kind != yaml.ScalarNode || item.Tag != "!!str" {
			return nil, fmt.Errorf("line %d: keys[%d].target.rewrite[%d] must be a string", line, keyIndex, i)
		}
		source, destination, ok := strings.Cut(item.Value, ":")
		if !ok || strings.Count(item.Value, ":") != 1 || !validRewritePrefix(source) || (destination != "-" && !validRewritePrefix(destination)) {
			return nil, fmt.Errorf("line %d: keys[%d].target.rewrite[%d] must be source:destination with an absolute escaped source path and an absolute escaped destination path or '-' to delete the prefix (no authority, query, fragment or whitespace)", line, keyIndex, i)
		}
		if destination == "-" {
			destination = ""
		}
		rules = append(rules, RewriteRule{Source: source, Destination: destination})
	}
	return rules, nil
}

func validRewritePrefix(prefix string) bool {
	if !strings.HasPrefix(prefix, "/") || strings.HasPrefix(prefix, "//") || strings.ContainsAny(prefix, "?#") || strings.ContainsFunc(prefix, func(r rune) bool {
		return unicode.IsSpace(r) || unicode.IsControl(r)
	}) {
		return false
	}
	decoded, err := url.PathUnescape(prefix)
	if err != nil {
		return false
	}
	// Reject characters that cannot be retained literally as an escaped path.
	u := url.URL{Path: decoded, RawPath: prefix}
	return u.EscapedPath() == prefix
}
