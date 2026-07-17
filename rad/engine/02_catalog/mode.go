package catalog

// Catalog management mode. Every database is either directly managed (the
// database is the source of truth; imperative catalog mutation over the
// wire is allowed) or schema-managed (a schema.rad document is the source
// of truth; the only mutation path is migration). The mode is decided once, when the database
// is first initialised, and never changes — adoption flows between modes are
// future work.
//
// The mode is catalog metadata, stored under the same meta prefix as the ID
// counter, so it is snapshot-consistent with everything else in the catalog.
// Enforcement is deliberately NOT here: the reconciler and the imperative
// API both drive the same catalog mutation surface, so the catalog cannot
// tell the channels apart. The transport gates its imperative endpoints on
// the stored mode; this file only records and reports it.

import (
	"context"
	"fmt"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
)

type Mode string

const (
	// ModeDirect: the live catalog is the source of truth. The default.
	ModeDirect Mode = "direct"
	// ModeSchema: schema.rad migrations are the source of truth.
	ModeSchema Mode = "schema"
)

const modeKey = "/rad/catalog/meta/mode"

// ParseMode validates an operator-supplied mode string.
func ParseMode(s string) (Mode, error) {
	switch Mode(s) {
	case ModeDirect, ModeSchema:
		return Mode(s), nil
	default:
		return "", fmt.Errorf("catalog: unknown catalog mode %q (direct or schema)", s)
	}
}

// Mode reports the database's catalog management mode. A database with no
// stored mode (never initialised through InitMode — embedded and test uses)
// is directly managed.
func (c *Catalog) Mode(ctx context.Context) (Mode, error) {
	return readMode(ctx, c.store)
}

func readMode(ctx context.Context, view kv.KV) (Mode, error) {
	raw, ok, err := view.Get(ctx, []byte(modeKey))
	if err != nil {
		return "", err
	}
	if !ok {
		return ModeDirect, nil
	}
	mode, err := ParseMode(string(raw))
	if err != nil {
		return "", fmt.Errorf("catalog: corrupt stored mode %q", raw)
	}
	return mode, nil
}

// InitMode settles the database's mode at boot and returns it. requested is
// the operator's choice ("" for no preference). On a database with no stored
// mode, the requested mode (or the direct default) is stamped and becomes
// permanent. On a database that already has one, the stored mode wins; an
// explicit request for a different mode is an error — the mode is set once
// and a mismatch means the operator is confused about which database this
// is. Loud beats silent.
func (c *Catalog) InitMode(ctx context.Context, requested Mode) (Mode, error) {
	var settled Mode
	err := c.transact(ctx, func(view kv.Txn) error {
		raw, ok, err := view.Get(ctx, []byte(modeKey))
		if err != nil {
			return err
		}
		if ok {
			stored, err := ParseMode(string(raw))
			if err != nil {
				return fmt.Errorf("catalog: corrupt stored mode %q", raw)
			}
			if requested != "" && requested != stored {
				return fmt.Errorf("catalog: this database is %s-managed and its mode is set once at creation; it cannot be changed to %s", stored, requested)
			}
			settled = stored
			return nil
		}
		settled = requested
		if settled == "" {
			settled = ModeDirect
		}
		return view.Put(ctx, []byte(modeKey), []byte(settled))
	})
	if err != nil {
		return "", err
	}
	return settled, nil
}
