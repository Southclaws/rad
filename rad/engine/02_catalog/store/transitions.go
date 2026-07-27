package store

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"strconv"
	"strings"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/reject"
)

const (
	writeProtocolPrefix           = "/rad/catalog/fence/table/"
	writeProtocolDefinitionPrefix = "/rad/catalog/object/write_protocol/"
	transitionPrefix              = "/rad/catalog/transition/"
	transitionWakeKey             = "/rad/catalog/meta/transition_seen"
	deltaPrefix                   = "/rad/catalog/transition_delta/"
	deltaSequencePrefix           = "/rad/catalog/transition_delta_sequence/"
	deltaAppliedPrefix            = "/rad/catalog/transition_delta_applied/"
	uniqueClaimPrefix             = "/rad/catalog/transition_unique_claim/"
	uniqueViolationPrefix         = "/rad/catalog/transition_unique_violation/"
)

func WriteProtocolKey(tableID string) []byte {
	return []byte(writeProtocolPrefix + tableID + "/write_protocol")
}

func WriteProtocolDefinitionKey(tableID string, generation uint64) []byte {
	return []byte(fmt.Sprintf("%s%s/definition/%020d", writeProtocolDefinitionPrefix, tableID, generation))
}

func WriteProtocolDefinitionRange(tableID string) (start, end []byte) {
	prefix := []byte(writeProtocolDefinitionPrefix + tableID + "/definition/")
	return prefix, keyenc.PrefixEnd(prefix)
}

func WriteProtocolGeneration(ctx context.Context, view kv.KV, tableID string) (uint64, bool, error) {
	raw, ok, err := view.Get(ctx, WriteProtocolKey(tableID))
	if err != nil || !ok {
		return 0, ok, err
	}
	generation, err := strconv.ParseUint(string(raw), 10, 64)
	if err != nil {
		return 0, false, fmt.Errorf("catalog: corrupt write protocol fence for table %q: %w", tableID, err)
	}
	return generation, true, nil
}

func ReadWriteProtocol(ctx context.Context, view kv.KV, table model.Table) (model.WriteProtocol, error) {
	raw, ok, err := view.Get(ctx, WriteProtocolKey(table.ID))
	if err != nil {
		return model.WriteProtocol{}, err
	}
	if !ok {
		if table.WriteProtocolGeneration != 0 {
			return model.WriteProtocol{}, fmt.Errorf("catalog: table %q expects missing write protocol generation %d", table.Name, table.WriteProtocolGeneration)
		}
		return model.WriteProtocol{TableID: table.ID, ReadyIndexes: readyIndexes(table)}, nil
	}
	generation, err := strconv.ParseUint(string(raw), 10, 64)
	if err != nil {
		return model.WriteProtocol{}, fmt.Errorf("catalog: corrupt write protocol fence for table %q: %w", table.Name, err)
	}
	if generation != table.WriteProtocolGeneration {
		return model.WriteProtocol{}, fmt.Errorf("catalog: table %q write protocol changed from generation %d to %d: %w",
			table.Name, table.WriteProtocolGeneration, generation, kv.ErrConflict)
	}
	definition, ok, err := view.Get(ctx, WriteProtocolDefinitionKey(table.ID, generation))
	if err != nil {
		return model.WriteProtocol{}, err
	}
	if !ok {
		return model.WriteProtocol{}, fmt.Errorf("catalog: table %q write protocol definition %d is missing", table.Name, generation)
	}
	protocol, err := decodeWriteProtocolDefinition(table.Name, definition)
	if err != nil {
		return model.WriteProtocol{}, err
	}
	if protocol.TableID != table.ID || protocol.Generation != generation {
		return model.WriteProtocol{}, fmt.Errorf("catalog: invalid write protocol definition for table %q at generation %d", table.Name, generation)
	}
	return protocol, nil
}

func decodeWriteProtocolDefinition(tableName string, definition []byte) (model.WriteProtocol, error) {
	protocol, err := decodeDurableJSON[model.WriteProtocol]("write protocol definition", tableName, definition)
	if err != nil {
		return model.WriteProtocol{}, err
	}
	return protocol, nil
}

func readyIndexes(table model.Table) []model.Index {
	indexes := make([]model.Index, 0, len(table.Indexes))
	for _, index := range table.Indexes {
		if index.Ready() {
			indexes = append(indexes, index)
		}
	}
	return indexes
}

// SaveWriteProtocol stores the immutable protocol definition as JSON and a
// separate small generation fence. Keeping the JSON encoding here makes the
// engine model's serialization boundary explicit.
func SaveWriteProtocol(ctx context.Context, view kv.KV, protocol model.WriteProtocol) error {
	if protocol.Generation == 0 {
		return fmt.Errorf("catalog: cannot publish write protocol generation zero for table %q", protocol.TableID)
	}
	protocol = canonicalWriteProtocol(protocol)
	raw, err := json.Marshal(protocol)
	if err != nil {
		return err
	}
	definitionKey := WriteProtocolDefinitionKey(protocol.TableID, protocol.Generation)
	if currentRaw, ok, err := view.Get(ctx, WriteProtocolKey(protocol.TableID)); err != nil {
		return err
	} else if ok {
		current, err := strconv.ParseUint(string(currentRaw), 10, 64)
		if err != nil {
			return fmt.Errorf("catalog: corrupt write protocol fence for table %q: %w", protocol.TableID, err)
		}
		if current != protocol.Generation {
			revision, err := CurrentRevision(ctx, view)
			if err != nil {
				return err
			}
			if err := QueueReclamation(ctx, view, model.Reclamation{
				ID:                    WriteProtocolReclamationID(protocol.TableID, current),
				Kind:                  model.ReclamationWriteProtocolDefinition,
				RetiredCatalogVersion: revision.Version + 1, TableID: protocol.TableID,
				WriteProtocolGeneration: current,
			}); err != nil {
				return err
			}
		}
	}
	if existing, ok, err := view.Get(ctx, definitionKey); err != nil {
		return err
	} else if ok && !bytes.Equal(existing, raw) {
		return fmt.Errorf("catalog: immutable write protocol %q generation %d already has different contents", protocol.TableID, protocol.Generation)
	} else if !ok {
		if err := view.Put(ctx, definitionKey, raw); err != nil {
			return err
		}
	}
	return view.Put(ctx, WriteProtocolKey(protocol.TableID), []byte(strconv.FormatUint(protocol.Generation, 10)))
}

func transitionKey(id string) []byte { return []byte(transitionPrefix + id) }

// SaveTransition stores restartable worker state as a JSON catalog record.
func SaveTransition(ctx context.Context, view kv.KV, transition model.SchemaTransition) error {
	if err := validateTransition(transition); err != nil {
		return err
	}
	raw, err := json.Marshal(transition)
	if err != nil {
		return err
	}
	if err := view.Put(ctx, transitionKey(transition.ID), raw); err != nil {
		return err
	}
	// A sticky point key avoids a range scan on every fresh Engine startup.
	// Once transition work has existed, startup scans and resumes its durable
	// records; terminal records intentionally remain useful diagnostics.
	return view.Put(ctx, []byte(transitionWakeKey), []byte{1})
}

func HasTransitionHistory(ctx context.Context, view kv.KV) (bool, error) {
	_, ok, err := view.Get(ctx, []byte(transitionWakeKey))
	return ok, err
}

func GetTransition(ctx context.Context, view kv.KV, id string) (model.SchemaTransition, bool, error) {
	raw, ok, err := view.Get(ctx, transitionKey(id))
	if err != nil || !ok {
		return model.SchemaTransition{}, ok, err
	}
	transition, err := decodeTransition(id, raw)
	if err != nil {
		return model.SchemaTransition{}, false, err
	}
	return transition, true, nil
}

func ListTransitions(ctx context.Context, view kv.KV) ([]model.SchemaTransition, error) {
	prefix := []byte(transitionPrefix)
	it, err := view.Scan(ctx, prefix, keyenc.PrefixEnd(prefix))
	if err != nil {
		return nil, err
	}
	defer it.Close()
	var transitions []model.SchemaTransition
	for it.Next() {
		id := strings.TrimPrefix(string(it.Key()), transitionPrefix)
		transition, err := decodeTransition(id, it.Value())
		if err != nil {
			return nil, err
		}
		transitions = append(transitions, transition)
	}
	return transitions, it.Err()
}

func decodeTransition(id string, raw []byte) (model.SchemaTransition, error) {
	transition, err := decodeDurableJSON[model.SchemaTransition]("transition", id, raw)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if transition.ID != id {
		return model.SchemaTransition{}, reject.Fail(
			reject.ReasonCatalogCorrupt,
			"catalog: transition key %q contains ID %q",
			id,
			transition.ID,
		)
	}
	if err := validateTransition(transition); err != nil {
		return model.SchemaTransition{}, reject.Mark(reject.ReasonCatalogCorrupt, err)
	}
	return transition, nil
}

func validateTransition(transition model.SchemaTransition) error {
	if transition.ID == "" || transition.Kind == "" || transition.State == "" ||
		transition.Generation == 0 {
		return fmt.Errorf("catalog: invalid transition checkpoint %+v", transition)
	}
	switch transition.State {
	case model.TransitionWaiting, model.TransitionBuilding, model.TransitionCatchingUp,
		model.TransitionValidating, model.TransitionReady, model.TransitionFailed,
		model.TransitionCancelled:
	default:
		return fmt.Errorf(
			"catalog: transition %q has invalid state %q",
			transition.ID,
			transition.State,
		)
	}
	// Wall time is diagnostic rather than a correctness clock and may move
	// backwards. Require timestamps to exist without imposing ordering.
	if transition.CreatedAt.IsZero() || transition.UpdatedAt.IsZero() {
		return fmt.Errorf("catalog: transition %q has invalid checkpoint timestamps", transition.ID)
	}
	if !transition.CompactedAt.IsZero() {
		switch transition.State {
		case model.TransitionReady, model.TransitionFailed, model.TransitionCancelled:
		default:
			return fmt.Errorf(
				"catalog: non-terminal transition %q has a compaction timestamp",
				transition.ID,
			)
		}
		return nil
	}
	if transition.TableID == "" || transition.TableSchemaID == 0 {
		return fmt.Errorf("catalog: transition %q has an incomplete table target", transition.ID)
	}
	if transition.DeltaHardLimit > 0 && transition.DeltaSoftLimit > transition.DeltaHardLimit {
		return fmt.Errorf(
			"catalog: transition %q has soft delta limit %d above hard limit %d",
			transition.ID,
			transition.DeltaSoftLimit,
			transition.DeltaHardLimit,
		)
	}
	return nil
}

func deltaSequenceKey(transitionID string) []byte {
	return []byte(deltaSequencePrefix + transitionID)
}

func deltaAppliedKey(transitionID string) []byte {
	return []byte(deltaAppliedPrefix + transitionID)
}

func DeltaKey(transitionID string, sequence uint64) []byte {
	return []byte(fmt.Sprintf("%s%s/%020d", deltaPrefix, transitionID, sequence))
}

func DeltaRange(transitionID string) (start, end []byte) {
	prefix := []byte(deltaPrefix + transitionID + "/")
	return prefix, keyenc.PrefixEnd(prefix)
}

func DeleteDeltaMetadata(ctx context.Context, view kv.KV, transitionID string) error {
	if err := view.Delete(ctx, deltaSequenceKey(transitionID)); err != nil {
		return err
	}
	return view.Delete(ctx, deltaAppliedKey(transitionID))
}

// AppendIndexDelta stores one JSON delta record and advances its numeric
// sequence in the same transaction as the foreground mutation.
func AppendIndexDelta(ctx context.Context, view kv.KV, transitionID string, hardLimit uint64, delta model.IndexDelta) (uint64, error) {
	key := deltaSequenceKey(transitionID)
	raw, ok, err := view.Get(ctx, key)
	if err != nil {
		return 0, err
	}
	var sequence uint64
	if ok {
		sequence, err = strconv.ParseUint(string(raw), 10, 64)
		if err != nil {
			return 0, fmt.Errorf("catalog: corrupt delta sequence for transition %q: %w", transitionID, err)
		}
	}
	if sequence == ^uint64(0) {
		return 0, fmt.Errorf("catalog: delta sequence for transition %q is exhausted", transitionID)
	}
	applied, err := deltaApplied(ctx, view, transitionID)
	if err != nil {
		return 0, err
	}
	lag := uint64(0)
	if sequence > applied {
		lag = sequence - applied
	}
	if hardLimit > 0 && lag >= hardLimit {
		return 0, reject.Fail(reject.ReasonTransitionBackpressure,
			"catalog: transition %q retained delta work reached hard limit %d; retry after the schema worker catches up",
			transitionID, hardLimit)
	}
	sequence++
	delta.ID = fmt.Sprintf("%s:%020d", transitionID, sequence)
	delta.Sequence = sequence
	deltaRaw, err := json.Marshal(delta)
	if err != nil {
		return 0, err
	}
	if err := view.Put(ctx, DeltaKey(transitionID, sequence), deltaRaw); err != nil {
		return 0, err
	}
	if err := view.Put(ctx, key, []byte(strconv.FormatUint(sequence, 10))); err != nil {
		return 0, err
	}
	return sequence, nil
}

func deltaApplied(ctx context.Context, view kv.KV, transitionID string) (uint64, error) {
	raw, ok, err := view.Get(ctx, deltaAppliedKey(transitionID))
	if err != nil || !ok {
		return 0, err
	}
	value, err := strconv.ParseUint(string(raw), 10, 64)
	if err != nil {
		return 0, fmt.Errorf("catalog: corrupt applied delta for transition %q: %w", transitionID, err)
	}
	return value, nil
}

func SaveDeltaApplied(ctx context.Context, view kv.KV, transitionID string, sequence uint64) error {
	return view.Put(ctx, deltaAppliedKey(transitionID), []byte(strconv.FormatUint(sequence, 10)))
}

func DeltaHighWater(ctx context.Context, view kv.KV, transitionID string) (uint64, error) {
	raw, ok, err := view.Get(ctx, deltaSequenceKey(transitionID))
	if err != nil || !ok {
		return 0, err
	}
	value, err := strconv.ParseUint(string(raw), 10, 64)
	if err != nil {
		return 0, fmt.Errorf("catalog: corrupt delta sequence for transition %q: %w", transitionID, err)
	}
	return value, nil
}

// DecodeIndexDelta validates a durable delta's key, identity, sequence, and
// operation together; callers must not trust the JSON value independently of
// the ordered key used to replay it.
func DecodeIndexDelta(transitionID string, key, raw []byte) (model.IndexDelta, error) {
	prefix := []byte(deltaPrefix + transitionID + "/")
	if !bytes.HasPrefix(key, prefix) {
		return model.IndexDelta{}, reject.Fail(
			reject.ReasonCatalogCorrupt,
			"catalog: index delta key %q is outside transition %q",
			key,
			transitionID,
		)
	}
	encodedSequence := string(key[len(prefix):])
	sequence, err := strconv.ParseUint(encodedSequence, 10, 64)
	if err != nil || encodedSequence != fmt.Sprintf("%020d", sequence) {
		return model.IndexDelta{}, reject.Fail(reject.ReasonCatalogCorrupt, "catalog: invalid index delta key %q", key)
	}
	delta, err := decodeDurableJSON[model.IndexDelta]("index delta", string(key), raw)
	if err != nil {
		return model.IndexDelta{}, err
	}
	if delta.Sequence != sequence || delta.ID != fmt.Sprintf("%s:%020d", transitionID, sequence) {
		return model.IndexDelta{}, reject.Fail(
			reject.ReasonCatalogCorrupt,
			"catalog: index delta key %q contains identity %q at sequence %d",
			key,
			delta.ID,
			delta.Sequence,
		)
	}
	switch delta.Operation {
	case model.IndexDeltaPut, model.IndexDeltaDelete:
	default:
		return model.IndexDelta{}, reject.Fail(
			reject.ReasonCatalogCorrupt,
			"catalog: index delta %q has unknown operation %q",
			delta.ID,
			delta.Operation,
		)
	}
	return delta, nil
}

func uniqueClaimKey(transitionID string, tuple, pk []byte) []byte {
	key := uniqueClaimTuplePrefix(transitionID, tuple)
	return append(key, pk...)
}

func uniqueClaimTuplePrefix(transitionID string, tuple []byte) []byte {
	key := []byte(uniqueClaimPrefix + transitionID + "/")
	key = binary.BigEndian.AppendUint64(key, uint64(len(tuple)))
	key = append(key, tuple...)
	return append(key, '/')
}

func UniqueClaimRange(transitionID string) (start, end []byte) {
	prefix := []byte(uniqueClaimPrefix + transitionID + "/")
	return prefix, keyenc.PrefixEnd(prefix)
}

func UniqueViolationRange(transitionID string) (start, end []byte) {
	prefix := []byte(uniqueViolationPrefix + transitionID + "/")
	return prefix, keyenc.PrefixEnd(prefix)
}

func uniqueViolationKey(transitionID string, tuple []byte) []byte {
	key := []byte(uniqueViolationPrefix + transitionID + "/")
	key = binary.BigEndian.AppendUint64(key, uint64(len(tuple)))
	return append(key, tuple...)
}

func PutUniqueClaim(ctx context.Context, view kv.KV, transitionID string, tuple, pk []byte) error {
	claim := model.UniqueIndexClaim{Tuple: bytes.Clone(tuple), PK: bytes.Clone(pk)}
	raw, err := json.Marshal(claim)
	if err != nil {
		return err
	}
	prefix := uniqueClaimTuplePrefix(transitionID, tuple)
	it, err := view.Scan(ctx, prefix, keyenc.PrefixEnd(prefix))
	if err != nil {
		return err
	}
	duplicate := false
	for it.Next() {
		existingClaim, err := decodeUniqueClaim(it.Value())
		if err != nil {
			_ = it.Close()
			return fmt.Errorf("catalog: corrupt unique claim %q: %w", it.Key(), err)
		}
		if !bytes.Equal(existingClaim.Tuple, tuple) || !bytes.Equal(existingClaim.PK, it.Key()[len(prefix):]) {
			_ = it.Close()
			return reject.Fail(
				reject.ReasonCatalogCorrupt,
				"catalog: unique claim key %q disagrees with its durable value",
				it.Key(),
			)
		}
		if !bytes.Equal(existingClaim.PK, pk) {
			duplicate = true
			break
		}
	}
	if err := it.Err(); err != nil {
		_ = it.Close()
		return err
	}
	if err := it.Close(); err != nil {
		return err
	}
	key := uniqueClaimKey(transitionID, tuple, pk)
	if existing, ok, err := view.Get(ctx, key); err != nil {
		return err
	} else if ok && !bytes.Equal(existing, raw) {
		return fmt.Errorf("catalog: unique claim %q has conflicting contents", key)
	}
	if err := view.Put(ctx, key, raw); err != nil {
		return err
	}
	if duplicate {
		return view.Put(ctx, uniqueViolationKey(transitionID, tuple), bytes.Clone(tuple))
	}
	return nil
}

func DeleteUniqueClaim(ctx context.Context, view kv.KV, transitionID string, tuple, pk []byte) error {
	if err := view.Delete(ctx, uniqueClaimKey(transitionID, tuple, pk)); err != nil {
		return err
	}
	prefix := uniqueClaimTuplePrefix(transitionID, tuple)
	it, err := view.Scan(ctx, prefix, keyenc.PrefixEnd(prefix))
	if err != nil {
		return err
	}
	remaining := 0
	for it.Next() {
		claim, err := decodeUniqueClaim(it.Value())
		if err != nil {
			_ = it.Close()
			return fmt.Errorf("catalog: corrupt unique claim %q: %w", it.Key(), err)
		}
		if !bytes.Equal(claim.Tuple, tuple) || !bytes.Equal(claim.PK, it.Key()[len(prefix):]) {
			_ = it.Close()
			return reject.Fail(
				reject.ReasonCatalogCorrupt,
				"catalog: unique claim key %q disagrees with its durable value",
				it.Key(),
			)
		}
		if bytes.Equal(claim.PK, pk) {
			continue
		}
		remaining++
		if remaining > 1 {
			break
		}
	}
	if err := it.Err(); err != nil {
		_ = it.Close()
		return err
	}
	if err := it.Close(); err != nil {
		return err
	}
	if remaining > 1 {
		return view.Put(ctx, uniqueViolationKey(transitionID, tuple), bytes.Clone(tuple))
	}
	return view.Delete(ctx, uniqueViolationKey(transitionID, tuple))
}

func decodeUniqueClaim(raw []byte) (model.UniqueIndexClaim, error) {
	return decodeDurableJSON[model.UniqueIndexClaim]("unique index claim", "", raw)
}

func FirstUniqueViolation(ctx context.Context, view kv.KV, transitionID string) ([]byte, bool, error) {
	start, end := UniqueViolationRange(transitionID)
	it, err := view.Scan(ctx, start, end)
	if err != nil {
		return nil, false, err
	}
	defer it.Close()
	if !it.Next() {
		return nil, false, it.Err()
	}
	return bytes.Clone(it.Value()), true, nil
}
