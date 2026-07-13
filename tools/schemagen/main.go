// Command schemagen converts a JSON Schema authored in YAML into JSON, and
// optionally publishes a copy for the website. The YAML is the source of
// truth; the JSON is a generated artifact (embedded by the server for
// validation, read by Schemancer, and served at the schema's $id URL).
//
// It is wired as a //go:generate helper in the protocol package; run it with
// `go generate ./rad/protocol` or `task protocol:generate`.
//
// Conversion goes through goccy/go-yaml, which follows YAML 1.2: bare tokens
// like `on`, `off`, `yes`, and `no` stay strings rather than being coerced to
// booleans (the YAML 1.1 behaviour that would silently rewrite a key like
// JoinNode's `on`). Object key order is not preserved, which is fine:
// authorship order lives in the YAML, the JSON is machine read.
package main

import (
	"bytes"
	"encoding/json"
	"flag"
	"log"
	"os"
	"path/filepath"

	yaml "github.com/goccy/go-yaml"
)

func main() {
	log.SetFlags(0)
	log.SetPrefix("schemagen: ")

	yamlPath := flag.String("yaml", "", "source YAML schema (the authored source of truth)")
	jsonPath := flag.String("json", "", "JSON schema to write (generated artifact)")
	webPath := flag.String("web", "", "optional path to publish a copy of the JSON for the website")
	flag.Parse()

	if *yamlPath == "" || *jsonPath == "" {
		log.Fatal("both -yaml and -json are required")
	}

	src, err := os.ReadFile(*yamlPath)
	if err != nil {
		log.Fatalf("read %s: %v", *yamlPath, err)
	}

	var doc any
	if err := yaml.Unmarshal(src, &doc); err != nil {
		log.Fatalf("parse %s: %v", *yamlPath, err)
	}

	var pretty bytes.Buffer
	enc := json.NewEncoder(&pretty)
	enc.SetIndent("", "  ")
	enc.SetEscapeHTML(false)
	if err := enc.Encode(doc); err != nil {
		log.Fatalf("encode JSON: %v", err)
	}
	out := pretty.Bytes()

	write(*jsonPath, out)
	if *webPath != "" {
		if err := os.MkdirAll(filepath.Dir(*webPath), 0o755); err != nil {
			log.Fatalf("mkdir %s: %v", filepath.Dir(*webPath), err)
		}
		write(*webPath, out)
	}
}

func write(path string, data []byte) {
	if err := os.WriteFile(path, data, 0o644); err != nil {
		log.Fatalf("write %s: %v", path, err)
	}
}
