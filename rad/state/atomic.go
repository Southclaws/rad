package state

import (
	"os"
	"path/filepath"
)

func atomicWrite(filename string, source []byte, mode os.FileMode) error {
	temporaryName, err := writeTemporary(filename, source, mode)
	if err != nil {
		return err
	}
	defer os.Remove(temporaryName)
	return os.Rename(temporaryName, filename)
}

func atomicCreate(filename string, source []byte, mode os.FileMode) error {
	temporaryName, err := writeTemporary(filename, source, mode)
	if err != nil {
		return err
	}
	defer os.Remove(temporaryName)
	return os.Link(temporaryName, filename)
}

func writeTemporary(filename string, source []byte, mode os.FileMode) (string, error) {
	directory := filepath.Dir(filename)
	if err := os.MkdirAll(directory, 0o755); err != nil {
		return "", err
	}
	temporary, err := os.CreateTemp(directory, ".rad-tmp-*")
	if err != nil {
		return "", err
	}
	temporaryName := temporary.Name()
	if err := temporary.Chmod(mode); err != nil {
		temporary.Close()
		os.Remove(temporaryName)
		return "", err
	}
	if _, err := temporary.Write(source); err != nil {
		temporary.Close()
		os.Remove(temporaryName)
		return "", err
	}
	if err := temporary.Sync(); err != nil {
		temporary.Close()
		os.Remove(temporaryName)
		return "", err
	}
	if err := temporary.Close(); err != nil {
		os.Remove(temporaryName)
		return "", err
	}
	return temporaryName, nil
}
