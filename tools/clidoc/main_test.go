package main

import (
	"strings"
	"testing"
)

func TestRender(t *testing.T) {
	page, err := render(document{
		Info: info{
			BinaryName:  "rad",
			Description: "A database.",
		},
		Commands: []command{{
			Name:        "serve",
			Description: "Run the server.",
			Flags: []option{
				{Ref: "#/components/flags/address"},
				{Name: "storage", Choices: []string{"memory", "file"}},
			},
		}},
		Components: components{Flags: map[string]option{
			"address": {
				Name:        "addr",
				Description: "Listen address.",
				Default:     "127.0.0.1:7237",
				EnvVar:      "RAD_ADDR",
			},
		}},
	})
	if err != nil {
		t.Fatal(err)
	}

	got := string(page)
	for _, want := range []string{
		"| `rad serve` | Run the server. |",
		"### `rad serve`",
		"| `--addr <value>` | Listen address. | `127.0.0.1:7237` | `RAD_ADDR` |",
		"| `--storage <memory\\|file>` |",
	} {
		if !strings.Contains(got, want) {
			t.Errorf("generated page does not contain %q\n%s", want, got)
		}
	}
}
