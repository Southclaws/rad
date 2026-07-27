package store

import (
	"context"
	"encoding/hex"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
)

const transitionViolationPrefix = "/rad/catalog/transition_violation/"

func transitionViolationKey(transitionID string, rowIdentity []byte) []byte {
	return []byte(transitionViolationPrefix + transitionID + "/" + hex.EncodeToString(rowIdentity))
}

func TransitionViolationRange(transitionID string) (start, end []byte) {
	prefix := []byte(transitionViolationPrefix + transitionID + "/")
	return prefix, keyenc.PrefixEnd(prefix)
}

func PutTransitionViolation(
	ctx context.Context,
	view kv.KV,
	transitionID string,
	rowIdentity []byte,
	cause string,
) error {
	return view.Put(ctx, transitionViolationKey(transitionID, rowIdentity), []byte(cause))
}

func DeleteTransitionViolation(
	ctx context.Context,
	view kv.KV,
	transitionID string,
	rowIdentity []byte,
) error {
	return view.Delete(ctx, transitionViolationKey(transitionID, rowIdentity))
}

func FirstTransitionViolation(
	ctx context.Context,
	view kv.KV,
	transitionID string,
) (rowIdentity []byte, cause string, ok bool, err error) {
	start, end := TransitionViolationRange(transitionID)
	it, err := view.Scan(ctx, start, end)
	if err != nil {
		return nil, "", false, err
	}
	defer it.Close()
	if !it.Next() {
		return nil, "", false, it.Err()
	}
	encoded := it.Key()[len(start):]
	rowIdentity = make([]byte, hex.DecodedLen(len(encoded)))
	n, err := hex.Decode(rowIdentity, encoded)
	if err != nil {
		return nil, "", false, err
	}
	return rowIdentity[:n], string(it.Value()), true, nil
}
