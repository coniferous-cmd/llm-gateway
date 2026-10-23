package proxy

import (
	"net/url"
	"reflect"
	"testing"

	"llm-gateway/internal/config"
)

func TestRewritePath(t *testing.T) {
	standard := []config.RewriteRule{{Source: "/api", Destination: "/v1"}}
	remove := []config.RewriteRule{{Source: "/api", Destination: ""}}
	for _, tt := range []struct {
		name, path, want string
		rules            []config.RewriteRule
	}{
		{"delete exact", "/api", "/", remove},
		{"delete trailing slash", "/api/", "/", remove},
		{"delete descendant", "/api/v1/messages", "/v1/messages", remove},
		{"delete once", "/api/api/models", "/api/models", remove},
		{"delete boundary", "/apix/models", "/apix/models", remove},
		{"delete not at start", "/other/api/models", "/other/api/models", remove},
		{"delete encoded suffix", "/api/a%2Fb%20c/%25/%2f", "/a%2Fb%20c/%25/%2f", remove},
		{"delete encoded boundary", "/api%2Fmodels", "/api%2Fmodels", remove},
		{"delete no cleaning", "/api/../a//b", "/../a//b", remove},
		{"delete root exact", "/", "/", []config.RewriteRule{{Source: "/", Destination: ""}}},
		{"delete root", "/models", "/models", []config.RewriteRule{{Source: "/", Destination: ""}}},
		{"delete trailing source", "/api/models", "/models", []config.RewriteRule{{Source: "/api/", Destination: ""}}},
		{"delete trailing source exact", "/api/", "/", []config.RewriteRule{{Source: "/api/", Destination: ""}}},
		{"delete no chaining", "/api/v1/models", "/v1/models", []config.RewriteRule{{Source: "/api", Destination: ""}, {Source: "/v1", Destination: "/v2"}}},
		{"exact", "/api", "/v1", standard},
		{"trailing slash", "/api/", "/v1/", standard},
		{"descendant", "/api/chat/completions", "/v1/chat/completions", standard},
		{"not segment", "/apix/models", "/apix/models", standard},
		{"not at start", "/other/api/models", "/other/api/models", standard},
		{"case sensitive", "/API/models", "/API/models", standard},
		{"not global replace", "/api/api/models", "/v1/api/models", standard},
		{"no cleaning", "/api/../a//b", "/v1/../a//b", standard},
		{"encoded suffix", "/api/a%2Fb%20c/%25/%2f", "/v1/a%2Fb%20c/%25/%2f", standard},
		{"encoded boundary", "/api%2Fmodels", "/api%2Fmodels", standard},
		{"encoded source differs", "/%61pi/models", "/%61pi/models", standard},
		{"no rules", "/api/a%2Fb", "/api/a%2Fb", nil},
		{"first not longest", "/api/admin/m", "/v1/admin/m", []config.RewriteRule{{Source: "/api", Destination: "/v1"}, {Source: "/api/admin", Destination: "/admin"}}},
		{"specific first", "/api/admin/m", "/admin/m", []config.RewriteRule{{Source: "/api/admin", Destination: "/admin"}, {Source: "/api", Destination: "/v1"}}},
		{"second prefix", "/openai/models", "/v1/models", []config.RewriteRule{{Source: "/api", Destination: "/v1"}, {Source: "/openai", Destination: "/v1"}}},
		{"no chaining", "/api/models", "/v1/models", []config.RewriteRule{{Source: "/api", Destination: "/v1"}, {Source: "/v1", Destination: "/v2"}}},
		{"root", "/models", "/v1/models", []config.RewriteRule{{Source: "/", Destination: "/v1/"}}},
		{"root exact", "/", "/v1/", []config.RewriteRule{{Source: "/", Destination: "/v1/"}}},
		{"literal trailing source", "/api/models", "/v1/models", []config.RewriteRule{{Source: "/api/", Destination: "/v1/"}}},
		{"trailing source exact", "/api/", "/v1/", []config.RewriteRule{{Source: "/api/", Destination: "/v1/"}}},
		{"trailing source unmatched", "/api", "/api", []config.RewriteRule{{Source: "/api/", Destination: "/v1/"}}},
		{"literal double slash", "/api/models", "/v1//models", []config.RewriteRule{{Source: "/api", Destination: "/v1/"}}},
		{"destination root", "/api/models", "/models", []config.RewriteRule{{Source: "/api/", Destination: "/"}}},
		{"escaped destination", "/api/models", "/v%20x/models", []config.RewriteRule{{Source: "/api", Destination: "/v%20x"}}},
		{"escaped source literal", "/a%2Fb/models", "/v1/models", []config.RewriteRule{{Source: "/a%2Fb", Destination: "/v1"}}},
	} {
		t.Run(tt.name, func(t *testing.T) {
			u, err := url.Parse(tt.path + "?next=/api&x=a%2Fb&x=2")
			if err != nil {
				t.Fatal(err)
			}
			before := *u
			rewritePath(u, tt.rules)
			if u.EscapedPath() != tt.want || u.RawQuery != before.RawQuery || u.ForceQuery != before.ForceQuery {
				t.Fatalf("URL = %s, want path %s and original query", u, tt.want)
			}
			decoded, _ := url.PathUnescape(tt.want)
			if u.Path != decoded {
				t.Fatalf("decoded path = %q, want %q", u.Path, decoded)
			}
			if tt.want == tt.path && !reflect.DeepEqual(*u, before) {
				t.Fatalf("unmatched URL changed: %+v -> %+v", before, *u)
			}
		})
	}
}

func TestRewritePreservesEmptyQuery(t *testing.T) {
	u, err := url.Parse("/api/models?")
	if err != nil {
		t.Fatal(err)
	}
	rewritePath(u, []config.RewriteRule{{Source: "/api", Destination: "/v1"}})
	if u.RequestURI() != "/v1/models?" || !u.ForceQuery {
		t.Fatal(u)
	}
}
