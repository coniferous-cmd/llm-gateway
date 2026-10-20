// Package proxy forwards authenticated requests without interpreting API payloads.
package proxy

import (
	"net"
	"net/http"
	"net/http/httputil"
	"net/url"
	"time"

	"llm-gateway/internal/keymap"
)

func New() http.Handler {
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.DialContext = (&net.Dialer{Timeout: 10 * time.Second, KeepAlive: 30 * time.Second}).DialContext
	transport.MaxIdleConnsPerHost = 16
	transport.ResponseHeaderTimeout = 30 * time.Second
	// Do not add Accept-Encoding or transparently decompress upstream responses.
	transport.DisableCompression = true
	p := &httputil.ReverseProxy{
		Transport:     transport,
		FlushInterval: -1,
		Director: func(r *http.Request) {
			target, _ := keymap.TargetFromContext(r.Context())
			base, _ := url.Parse(target.BaseURL) // Validated once at startup.
			r.URL.Scheme, r.URL.Host = base.Scheme, base.Host
			// Host must identify the upstream virtual host, including any port.
			r.Host = base.Host
			// Path, RawPath, RawQuery, body and other application headers stay intact.
			if target.APIKey == "" {
				r.Header.Del("Authorization")
			} else {
				r.Header.Set("Authorization", "Bearer "+target.APIKey)
			}
		},
		ErrorHandler: func(w http.ResponseWriter, _ *http.Request, _ error) {
			http.Error(w, "Bad Gateway", http.StatusBadGateway)
		},
	}
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if _, ok := keymap.TargetFromContext(r.Context()); !ok {
			http.Error(w, "Bad Gateway", http.StatusBadGateway)
			return
		}
		// ReverseProxy reads RemoteAddr from its input, not the Director's clone.
		// Clear it before ServeHTTP to preserve existing XFF without appending.
		r = r.Clone(r.Context())
		r.RemoteAddr = ""
		p.ServeHTTP(w, r)
	})
}
