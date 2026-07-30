package main

import (
	"errors"
	"flag"
	"fmt"
	"net/http"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"runtime"
	"strings"
	"time"
)

const localRadURL = "rad://127.0.0.1:7237"

func runDemo(arguments []string) error {
	flags := flag.NewFlagSet("rad-demo", flag.ContinueOnError)
	local := flags.Bool("local", false, "build and run a fresh local Rad server")
	stay := flags.Bool("stay", false, "keep the local server running after the demo completes")
	if err := flags.Parse(arguments); err != nil {
		return err
	}
	if flags.NArg() != 0 {
		return fmt.Errorf("unexpected arguments: %s", strings.Join(flags.Args(), " "))
	}
	if *local || *stay {
		return runLocalDemo(*stay)
	}

	url := os.Getenv("RAD_URL")
	if url == "" {
		url = localRadURL
	}
	return runTracker(url)
}

func runLocalDemo(stay bool) error {
	root, err := repositoryRoot()
	if err != nil {
		return err
	}

	data := filepath.Join(root, "examples", "demo", "data")
	if err := os.RemoveAll(data); err != nil {
		return fmt.Errorf("remove demo data: %w", err)
	}
	if err := command(root, "cargo", "build", "--locked").Run(); err != nil {
		return fmt.Errorf("build Rad: %w", err)
	}

	binary := radBinary(root, os.Getenv("CARGO_TARGET_DIR"), runtime.GOOS)
	logPath := filepath.Join(os.TempDir(), "rad-demo-serve.log")
	logFile, err := os.Create(logPath)
	if err != nil {
		return fmt.Errorf("create server log: %w", err)
	}
	defer logFile.Close()

	server := command(root, binary,
		"serve",
		"--addr", "127.0.0.1:7237",
		"--storage", "file",
		"--db", data,
		"--storage-path", "database",
		"--catalog-mode", "schema",
	)
	server.Stdout = logFile
	server.Stderr = logFile
	if err := server.Start(); err != nil {
		return fmt.Errorf("start Rad: %w", err)
	}
	exited := make(chan error, 1)
	go func() { exited <- server.Wait() }()
	defer stop(server, exited)

	if err := waitUntilReady(exited, 30*time.Second); err != nil {
		return fmt.Errorf("start Rad (log: %s): %w", logPath, err)
	}

	config := filepath.Join(root, "examples", "demo", "rad.config.yaml")
	schema := filepath.Join(root, "examples", "demo", "rad.schema.yaml")
	migrate := command(root, binary,
		"schema", "--config", config, "--file", schema,
		"migrate", "--no-generate",
	)
	if err := migrate.Run(); err != nil {
		return fmt.Errorf("migrate demo schema: %w", err)
	}
	if err := runTracker(localRadURL); err != nil {
		return fmt.Errorf("run tracker demo: %w", err)
	}
	if !stay {
		return nil
	}

	fmt.Printf("Rad is still serving on http://127.0.0.1:7237 (log: %s)\n", logPath)
	interrupt := make(chan os.Signal, 1)
	signal.Notify(interrupt, os.Interrupt)
	defer signal.Stop(interrupt)
	select {
	case <-interrupt:
		return nil
	case err := <-exited:
		return fmt.Errorf("Rad exited: %w", err)
	}
}

func repositoryRoot() (string, error) {
	output, err := exec.Command("git", "rev-parse", "--show-toplevel").Output()
	if err != nil {
		return "", fmt.Errorf("find repository root: %w", err)
	}
	return strings.TrimSpace(string(output)), nil
}

func radBinary(root, target, goos string) string {
	if target == "" {
		target = filepath.Join(root, "target")
	} else if !filepath.IsAbs(target) {
		target = filepath.Join(root, target)
	}
	name := "rad"
	if goos == "windows" {
		name += ".exe"
	}
	return filepath.Join(target, "debug", name)
}

func command(directory, name string, arguments ...string) *exec.Cmd {
	cmd := exec.Command(name, arguments...)
	cmd.Dir = directory
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	return cmd
}

func waitUntilReady(exited <-chan error, timeout time.Duration) error {
	deadline := time.NewTimer(timeout)
	defer deadline.Stop()
	ticker := time.NewTicker(200 * time.Millisecond)
	defer ticker.Stop()
	client := &http.Client{Timeout: time.Second}
	for {
		select {
		case err := <-exited:
			return fmt.Errorf("server exited: %w", err)
		case <-deadline.C:
			return errors.New("timed out waiting for /health")
		case <-ticker.C:
			response, err := client.Get("http://127.0.0.1:7237/health")
			if err == nil {
				response.Body.Close()
				if response.StatusCode >= 200 && response.StatusCode < 300 {
					return nil
				}
			}
		}
	}
}

func stop(server *exec.Cmd, exited <-chan error) {
	if server.Process == nil {
		return
	}
	_ = server.Process.Kill()
	select {
	case <-exited:
	case <-time.After(5 * time.Second):
	}
}
