package main

import (
	"path/filepath"
	"testing"
)

func TestRadBinaryUsesPlatformExecutableAndConfiguredTarget(t *testing.T) {
	root := filepath.Join("workspace", "rad")
	windows := radBinary(root, "build", "windows")
	if filepath.Base(windows) != "rad.exe" {
		t.Fatalf("Windows binary = %q", windows)
	}
	if filepath.Dir(filepath.Dir(windows)) != filepath.Join(root, "build") {
		t.Fatalf("relative target was not rooted at the repository: %q", windows)
	}

	unix := radBinary(root, "", "linux")
	if filepath.Base(unix) != "rad" {
		t.Fatalf("Unix binary = %q", unix)
	}
}
