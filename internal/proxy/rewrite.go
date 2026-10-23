package proxy

import (
	"net/url"
	"strings"

	"llm-gateway/internal/config"
)

// rewritePath applies at most one rule to the escaped path. Query fields and
// unmatched path representations are deliberately left untouched.
func rewritePath(u *url.URL, rules []config.RewriteRule) {
	if len(rules) == 0 {
		return
	}
	path := u.EscapedPath()
	for _, rule := range rules {
		if !strings.HasPrefix(path, rule.Source) {
			continue
		}
		suffix := path[len(rule.Source):]
		if suffix != "" && !strings.HasSuffix(rule.Source, "/") && suffix[0] != '/' {
			continue
		}
		escaped := rule.Destination + suffix
		if rule.Destination == "" && !strings.HasPrefix(escaped, "/") {
			// Prefix deletion must still yield an absolute HTTP path, even
			// for an exact match or a source ending in a slash.
			escaped = "/" + escaped
		}
		if escaped == path {
			return // Still stop at this match, preserving the original URL form.
		}
		// Both prefixes were validated at startup and EscapedPath always
		// returns a valid escaped representation, so unescaping cannot fail.
		decoded, _ := url.PathUnescape(escaped)
		u.Path, u.RawPath = decoded, escaped
		return
	}
}
