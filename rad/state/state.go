// Package state manages the accepted schema state stored in a Rad project.
// The database remains authoritative; this package writes only server-returned
// accepted schemas and refuses to overwrite divergent history.
package state

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"time"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/schema"
	"github.com/Southclaws/rad/rad/protocol"
)

const FormatVersion = 1

type Lock struct {
	FormatVersion int    `json:"format_version"`
	SchemaVersion uint64 `json:"schema_version"`
	SchemaHash    string `json:"schema_hash"`
	Snapshot      string `json:"snapshot"`
}

type Accepted struct {
	Lock   Lock
	Source []byte
	Schema catalog.Schema
}

type DivergenceError struct {
	Path     string
	Expected string
	Actual   string
}

func (e *DivergenceError) Error() string {
	return fmt.Sprintf("schema history diverged at %s: expected %s, found %s", e.Path, e.Expected, e.Actual)
}

type Store struct {
	Root string
}

func New(root string) Store { return Store{Root: root} }

func SnapshotName(version uint64) string {
	return fmt.Sprintf("%08d.rad.schema.yaml", version)
}

func (s Store) LockPath() string { return filepath.Join(s.Root, "schema.lock.json") }

func (s Store) SnapshotPath(version uint64) string {
	return filepath.Join(s.Root, "changelog", SnapshotName(version))
}

func (s Store) Load() (Accepted, error) {
	raw, err := os.ReadFile(s.LockPath())
	if err != nil {
		return Accepted{}, err
	}
	var lock Lock
	if err := json.Unmarshal(raw, &lock); err != nil {
		return Accepted{}, fmt.Errorf("%s: %w", s.LockPath(), err)
	}
	if lock.FormatVersion != FormatVersion {
		return Accepted{}, fmt.Errorf("%s: unsupported format_version %d", s.LockPath(), lock.FormatVersion)
	}
	wantSnapshot := filepath.ToSlash(filepath.Join("changelog", SnapshotName(lock.SchemaVersion)))
	if lock.Snapshot != wantSnapshot {
		return Accepted{}, fmt.Errorf("%s: snapshot is %q, want %q", s.LockPath(), lock.Snapshot, wantSnapshot)
	}
	snapshotPath, err := s.resolveSnapshot(lock.Snapshot)
	if err != nil {
		return Accepted{}, err
	}
	source, err := os.ReadFile(snapshotPath)
	if err != nil {
		return Accepted{}, err
	}
	canonical, hash, err := ParseSnapshot(snapshotPath, source)
	if err != nil {
		return Accepted{}, err
	}
	if hash != lock.SchemaHash {
		return Accepted{}, &DivergenceError{Path: snapshotPath, Expected: lock.SchemaHash, Actual: hash}
	}
	return Accepted{Lock: lock, Source: source, Schema: canonical}, nil
}

func (s Store) WriteAccepted(server protocol.SchemaState) (Accepted, error) {
	accepted, err := s.WriteSnapshot(server)
	if err != nil {
		return Accepted{}, err
	}
	if err := s.WriteLock(accepted.Lock); err != nil {
		return Accepted{}, err
	}
	return accepted, nil
}

func (s Store) WriteSnapshot(server protocol.SchemaState) (Accepted, error) {
	canonical, err := canonicalSchema(server.Schema)
	if err != nil {
		return Accepted{}, err
	}
	hash, err := canonical.Hash()
	if err != nil {
		return Accepted{}, err
	}
	if hash != server.SchemaHash {
		return Accepted{}, &DivergenceError{
			Path: "server accepted schema", Expected: server.SchemaHash, Actual: hash,
		}
	}
	source, err := schema.Render(canonical)
	if err != nil {
		return Accepted{}, err
	}
	snapshotPath := s.SnapshotPath(server.SchemaVersion)
	if err := os.MkdirAll(filepath.Dir(snapshotPath), 0o755); err != nil {
		return Accepted{}, err
	}
	if existing, err := os.ReadFile(snapshotPath); err == nil {
		_, existingHash, parseErr := ParseSnapshot(snapshotPath, existing)
		if parseErr != nil {
			return Accepted{}, parseErr
		}
		if existingHash != server.SchemaHash {
			return Accepted{}, &DivergenceError{
				Path: snapshotPath, Expected: server.SchemaHash, Actual: existingHash,
			}
		}
		source = existing
	} else if !errors.Is(err, os.ErrNotExist) {
		return Accepted{}, err
	} else if err := atomicCreate(snapshotPath, source, 0o644); err != nil {
		if !errors.Is(err, os.ErrExist) {
			return Accepted{}, err
		}
		existing, readErr := os.ReadFile(snapshotPath)
		if readErr != nil {
			return Accepted{}, readErr
		}
		_, existingHash, parseErr := ParseSnapshot(snapshotPath, existing)
		if parseErr != nil {
			return Accepted{}, parseErr
		}
		if existingHash != server.SchemaHash {
			return Accepted{}, &DivergenceError{
				Path: snapshotPath, Expected: server.SchemaHash, Actual: existingHash,
			}
		}
		source = existing
	}

	lock := Lock{
		FormatVersion: FormatVersion,
		SchemaVersion: server.SchemaVersion,
		SchemaHash:    server.SchemaHash,
		Snapshot:      filepath.ToSlash(filepath.Join("changelog", SnapshotName(server.SchemaVersion))),
	}
	return Accepted{Lock: lock, Source: source, Schema: canonical}, nil
}

func (s Store) WriteLock(lock Lock) error {
	lockSource, err := json.MarshalIndent(lock, "", "  ")
	if err != nil {
		return err
	}
	lockSource = append(lockSource, '\n')
	if err := atomicWrite(s.LockPath(), lockSource, 0o644); err != nil {
		return err
	}
	return nil
}

func (s Store) WriteDesired(filename string, source []byte) error {
	if _, _, err := ParseSnapshot(filename, source); err != nil {
		return err
	}
	return atomicWrite(filename, source, 0o644)
}

func (s Store) BackupDesired(filename string, now time.Time) (string, error) {
	source, err := os.ReadFile(filename)
	if err != nil {
		return "", err
	}
	directory := filepath.Join(s.Root, "backups")
	if err := os.MkdirAll(directory, 0o755); err != nil {
		return "", err
	}
	name := now.UTC().Format("20060102T150405Z") + ".rad.schema.yaml"
	path := filepath.Join(directory, name)
	if _, err := os.Stat(path); err == nil {
		return "", fmt.Errorf("backup already exists: %s", path)
	} else if !errors.Is(err, os.ErrNotExist) {
		return "", err
	}
	if err := atomicCreate(path, source, 0o644); err != nil {
		if errors.Is(err, os.ErrExist) {
			return "", fmt.Errorf("backup already exists: %s", path)
		}
		return "", err
	}
	return path, nil
}

func ParseSnapshot(filename string, source []byte) (catalog.Schema, string, error) {
	parsed, err := schema.Parse(filename, source)
	if err != nil {
		return catalog.Schema{}, "", err
	}
	canonical := parsed.Canonical()
	hash, err := canonical.Hash()
	return canonical, hash, err
}

func MatchesAccepted(filename string, source []byte, accepted Accepted) (bool, string, error) {
	_, hash, err := ParseSnapshot(filename, source)
	if err != nil {
		return false, "", err
	}
	return hash == accepted.Lock.SchemaHash, hash, nil
}

func (s Store) resolveSnapshot(relative string) (string, error) {
	clean := filepath.Clean(filepath.FromSlash(relative))
	if clean == "." || filepath.IsAbs(clean) || clean == ".." || len(clean) >= 3 && clean[:3] == ".."+string(filepath.Separator) {
		return "", fmt.Errorf("%s: snapshot path escapes rad.state: %q", s.LockPath(), relative)
	}
	return filepath.Join(s.Root, clean), nil
}

func canonicalSchema(document protocol.SchemaDocument) (catalog.Schema, error) {
	definitions := make([]catalog.TableDef, len(document.Tables))
	for i, table := range document.Tables {
		definition := catalog.TableDef{
			ID: catalog.SchemaID(table.ID), Name: table.Name, PrimaryKey: table.PrimaryKey,
		}
		for _, column := range table.Columns {
			defaultValue, err := canonicalDefault(column)
			if err != nil {
				return catalog.Schema{}, err
			}
			definition.Columns = append(definition.Columns, catalog.ColumnDef{
				ID: catalog.SchemaID(column.ID), Name: column.Name, Type: catalog.Type(column.Type),
				Nullable: column.Nullable, Format: column.Format, Default: defaultValue,
			})
		}
		for _, index := range table.Indexes {
			definition.Indexes = append(definition.Indexes, catalog.IndexDef{
				Name: index.Name, Columns: index.Columns, Unique: index.Unique,
			})
		}
		for _, foreignKey := range table.ForeignKeys {
			definition.ForeignKeys = append(definition.ForeignKeys, catalog.ForeignKeyDef{
				Name: foreignKey.Name, Columns: foreignKey.Columns,
				RefTable: foreignKey.RefTable, RefColumns: foreignKey.RefColumns,
			})
		}
		definitions[i] = definition
	}
	return catalog.SchemaFromDefinitions(definitions), nil
}

func canonicalDefault(column protocol.ColumnDef) (*catalog.Default, error) {
	if column.Default == nil {
		return nil, nil
	}
	if column.Default.Func != "" {
		return &catalog.Default{Func: catalog.DefaultFunc(column.Default.Func)}, nil
	}
	value := column.Default.Value
	switch catalog.Type(column.Type) {
	case catalog.TypeText:
		text, ok := value.(string)
		if !ok {
			return nil, fmt.Errorf("schema state: column %s default is %T, want string", column.Name, value)
		}
		return &catalog.Default{Text: text}, nil
	case catalog.TypeBool:
		boolean, ok := value.(bool)
		if !ok {
			return nil, fmt.Errorf("schema state: column %s default is %T, want bool", column.Name, value)
		}
		return &catalog.Default{Bool: boolean}, nil
	case catalog.TypeInt64:
		var number json.Number
		switch typed := value.(type) {
		case json.Number:
			number = typed
		case float64:
			number = json.Number(fmt.Sprint(typed))
		default:
			number = json.Number(fmt.Sprint(typed))
		}
		integer, err := number.Int64()
		if err != nil {
			return nil, fmt.Errorf("schema state: column %s integer default: %w", column.Name, err)
		}
		return &catalog.Default{Int64: integer}, nil
	case catalog.TypeFloat64:
		number := json.Number(fmt.Sprint(value))
		floating, err := number.Float64()
		if err != nil {
			return nil, fmt.Errorf("schema state: column %s float default: %w", column.Name, err)
		}
		return &catalog.Default{Float64: floating}, nil
	default:
		return nil, fmt.Errorf("schema state: column %s has unsupported type %q", column.Name, column.Type)
	}
}
