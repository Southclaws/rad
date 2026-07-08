// These tests document the rad:// connection URI: scheme mapping, the
// default port (7237, set in stone), and what is rejected.
package protocol

import (
	"strings"
	"testing"
)

func TestParseURL(t *testing.T) {
	cases := map[string]string{
		"rad://localhost":            "http://localhost:7237",
		"rad://localhost:9000":       "http://localhost:9000",
		"rad://db.internal":          "http://db.internal:7237",
		"rads://db.example.com":      "https://db.example.com:7237",
		"rads://db.example.com:8443": "https://db.example.com:8443",
		"rad://10.0.0.5":             "http://10.0.0.5:7237",
		"rad://[::1]":                "http://[::1]:7237",
		"rad://[::1]:9000":           "http://[::1]:9000",
		"rad://localhost/":           "http://localhost:7237",
	}
	for in, want := range cases {
		got, err := ParseURL(in)
		if err != nil {
			t.Errorf("ParseURL(%q): %v", in, err)
			continue
		}
		if got != want {
			t.Errorf("ParseURL(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestParseURLRejects(t *testing.T) {
	cases := map[string]string{
		"http://localhost:7237":    "rad:// or rads://",
		"postgres://localhost":     "rad:// or rads://",
		"rad://":                   "no host",
		"rad://user:pw@localhost":  "rad(s)://host[:port]",
		"rad://localhost/db":       "rad(s)://host[:port]",
		"rad://localhost?tls=true": "rad(s)://host[:port]",
	}
	for in, wantErr := range cases {
		_, err := ParseURL(in)
		if err == nil || !strings.Contains(err.Error(), wantErr) {
			t.Errorf("ParseURL(%q) = %v, want error containing %q", in, err, wantErr)
		}
	}
}

func TestDefaultPortIsSetInStone(t *testing.T) {
	if DefaultPort != 7237 {
		t.Fatalf("DefaultPort = %d — 7237 is set in stone", DefaultPort)
	}
}
