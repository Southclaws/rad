package store

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"

	"github.com/Southclaws/rad/rad/engine/reject"
)

func decodeDurableJSON[T any](kind, id string, raw []byte) (T, error) {
	var value T
	decoder := json.NewDecoder(bytes.NewReader(raw))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&value); err != nil {
		return value, reject.Mark(reject.ReasonCatalogCorrupt, durableJSONError(kind, id, err))
	}
	if err := decoder.Decode(&struct{}{}); err != io.EOF {
		return value, reject.Mark(
			reject.ReasonCatalogCorrupt,
			durableJSONError(kind, id, fmt.Errorf("trailing JSON value")),
		)
	}
	return value, nil
}

func durableJSONError(kind, id string, err error) error {
	if id == "" {
		return fmt.Errorf("catalog: corrupt %s: %w", kind, err)
	}
	return fmt.Errorf("catalog: corrupt %s %q: %w", kind, id, err)
}
