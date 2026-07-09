package main

import (
	"fmt"
	"os"
	"path/filepath"

	"rad/protocol"
	"rad/rad/01_kv/kvslate"
)

// Config is Rad's server configuration, sourced from environment variables
// (flags override where offered):
//
//	RAD_ADDR         listen address           (default ":7237")
//	RAD_STORAGE      memory | file | s3       (default "file")
//	RAD_DATA_DIR     file: store directory    (default "data")
//	RAD_S3_BUCKET    s3: bucket name          (required for s3)
//	RAD_S3_PREFIX    s3: key prefix / db path (default "rad")
//	RAD_S3_REGION    s3: region               (falls back to AWS_REGION)
//	RAD_S3_ENDPOINT  s3: custom endpoint      (MinIO/R2; falls back to AWS_ENDPOINT_URL)
//
// Credentials use the standard AWS environment (AWS_ACCESS_KEY_ID,
// AWS_SECRET_ACCESS_KEY, AWS_SESSION_TOKEN, …), read by the object store.
type Config struct {
	Addr    string
	Storage string // memory | file | s3

	DataDir string

	S3Bucket   string
	S3Prefix   string
	S3Region   string
	S3Endpoint string
}

func envOr(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

// LoadConfig reads the environment.
func LoadConfig() Config {
	return Config{
		Addr:       envOr("RAD_ADDR", fmt.Sprintf(":%d", protocol.DefaultPort)),
		Storage:    envOr("RAD_STORAGE", "file"),
		DataDir:    envOr("RAD_DATA_DIR", "data"),
		S3Bucket:   os.Getenv("RAD_S3_BUCKET"),
		S3Prefix:   envOr("RAD_S3_PREFIX", "rad"),
		S3Region:   os.Getenv("RAD_S3_REGION"),
		S3Endpoint: os.Getenv("RAD_S3_ENDPOINT"),
	}
}

// OpenStorage opens the configured SlateDB backend. Everything is SlateDB;
// the backends differ only by object-store URL.
func (c Config) OpenStorage() (*kvslate.Store, string, error) {
	switch c.Storage {
	case "memory":
		store, err := kvslate.Open("rad", "memory:///")
		return store, "memory:///", err

	case "file":
		dir, err := filepath.Abs(c.DataDir)
		if err != nil {
			return nil, "", err
		}
		if err := os.MkdirAll(dir, 0o755); err != nil {
			return nil, "", err
		}
		store, err := kvslate.Open(dir, "file://")
		return store, dir, err

	case "s3":
		if c.S3Bucket == "" {
			return nil, "", fmt.Errorf("RAD_STORAGE=s3 requires RAD_S3_BUCKET")
		}
		// The object store reads AWS_* environment variables; map RAD_S3_*
		// overrides onto them so one prefix configures everything.
		if c.S3Region != "" {
			os.Setenv("AWS_REGION", c.S3Region)
		}
		if c.S3Endpoint != "" {
			os.Setenv("AWS_ENDPOINT_URL", c.S3Endpoint)
		}
		url := "s3://" + c.S3Bucket
		store, err := kvslate.Open(c.S3Prefix, url)
		return store, url + "/" + c.S3Prefix, err

	default:
		return nil, "", fmt.Errorf("unknown RAD_STORAGE %q (memory, file, or s3)", c.Storage)
	}
}
