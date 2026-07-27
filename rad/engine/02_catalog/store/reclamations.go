package store

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/reject"
)

const (
	reclamationPrefix  = "/rad/catalog/reclamation/"
	reclamationWakeKey = "/rad/catalog/meta/reclamation_seen"
)

func reclamationKey(id string) []byte { return []byte(reclamationPrefix + id) }

// QueueReclamation writes a new durable reclamation record. Re-queuing the
// identical target is idempotent, which lets catalog publication and recovery
// safely reconstruct candidates without creating duplicate physical work.
func QueueReclamation(ctx context.Context, view kv.KV, reclamation model.Reclamation) error {
	if reclamation.State == "" {
		reclamation.State = model.ReclamationPending
	}
	if reclamation.Generation == 0 {
		reclamation.Generation = 1
	}
	if reclamation.CreatedAt.IsZero() {
		reclamation.CreatedAt = time.Now().UTC()
	}
	if reclamation.UpdatedAt.IsZero() {
		reclamation.UpdatedAt = reclamation.CreatedAt
	}
	if err := validateReclamation(reclamation); err != nil {
		return err
	}
	raw, err := json.Marshal(reclamation)
	if err != nil {
		return err
	}
	key := reclamationKey(reclamation.ID)
	existing, ok, err := view.Get(ctx, key)
	if err != nil {
		return err
	}
	if ok {
		current, err := decodeReclamation(reclamation.ID, existing)
		if err != nil {
			return err
		}
		if !current.CompactedAt.IsZero() && current.ID == reclamation.ID && current.Kind == reclamation.Kind {
			return nil
		}
		if sameReclamationTarget(current, reclamation) {
			return nil
		}
		return fmt.Errorf("catalog: reclamation %q already exists with different contents", reclamation.ID)
	}
	if err := view.Put(ctx, key, raw); err != nil {
		return err
	}
	// This sticky marker makes a fresh database's Engine construction a
	// point-read rather than an empty range scan. Once reclamation has ever
	// existed, startup conservatively scans durable records for unfinished
	// work; compacting the tiny history and marker is a later concern.
	return view.Put(ctx, []byte(reclamationWakeKey), []byte{1})
}

func HasReclamationHistory(ctx context.Context, view kv.KV) (bool, error) {
	_, ok, err := view.Get(ctx, []byte(reclamationWakeKey))
	return ok, err
}

func sameReclamationTarget(a, b model.Reclamation) bool {
	if a.ID != b.ID || a.Kind != b.Kind || a.RetiredCatalogVersion != b.RetiredCatalogVersion ||
		a.TableID != b.TableID || a.TableSchemaID != b.TableSchemaID || a.ColumnID != b.ColumnID ||
		a.IndexID != b.IndexID || a.DefinitionGeneration != b.DefinitionGeneration ||
		a.WriteProtocolGeneration != b.WriteProtocolGeneration || a.TransitionID != b.TransitionID ||
		len(a.IndexIDs) != len(b.IndexIDs) {
		return false
	}
	for i := range a.IndexIDs {
		if a.IndexIDs[i] != b.IndexIDs[i] {
			return false
		}
	}
	return true
}

func SaveReclamation(ctx context.Context, view kv.KV, reclamation model.Reclamation) error {
	if err := validateReclamation(reclamation); err != nil {
		return err
	}
	raw, err := json.Marshal(reclamation)
	if err != nil {
		return err
	}
	return view.Put(ctx, reclamationKey(reclamation.ID), raw)
}

func GetReclamation(ctx context.Context, view kv.KV, id string) (model.Reclamation, bool, error) {
	raw, ok, err := view.Get(ctx, reclamationKey(id))
	if err != nil || !ok {
		return model.Reclamation{}, ok, err
	}
	reclamation, err := decodeReclamation(id, raw)
	if err != nil {
		return model.Reclamation{}, false, err
	}
	return reclamation, true, nil
}

func ListReclamations(ctx context.Context, view kv.KV) ([]model.Reclamation, error) {
	prefix := []byte(reclamationPrefix)
	it, err := view.Scan(ctx, prefix, keyenc.PrefixEnd(prefix))
	if err != nil {
		return nil, err
	}
	defer it.Close()
	var reclamations []model.Reclamation
	for it.Next() {
		id := strings.TrimPrefix(string(it.Key()), reclamationPrefix)
		reclamation, err := decodeReclamation(id, it.Value())
		if err != nil {
			return nil, err
		}
		reclamations = append(reclamations, reclamation)
	}
	return reclamations, it.Err()
}

func decodeReclamation(id string, raw []byte) (model.Reclamation, error) {
	reclamation, err := decodeDurableJSON[model.Reclamation]("reclamation", id, raw)
	if err != nil {
		return model.Reclamation{}, err
	}
	if reclamation.ID != id {
		return model.Reclamation{}, reject.Fail(
			reject.ReasonCatalogCorrupt,
			"catalog: reclamation key %q contains ID %q",
			id,
			reclamation.ID,
		)
	}
	if err := validateReclamation(reclamation); err != nil {
		return model.Reclamation{}, reject.Mark(reject.ReasonCatalogCorrupt, err)
	}
	return reclamation, nil
}

func validateReclamation(reclamation model.Reclamation) error {
	if reclamation.ID == "" || reclamation.Kind == "" || reclamation.State == "" || reclamation.Generation == 0 {
		return fmt.Errorf("catalog: invalid reclamation checkpoint %+v", reclamation)
	}
	switch reclamation.State {
	case model.ReclamationPending, model.ReclamationReclaiming, model.ReclamationReclaimed, model.ReclamationFailed:
	default:
		return fmt.Errorf("catalog: reclamation %q has invalid state %q", reclamation.ID, reclamation.State)
	}
	if reclamation.RetiredCatalogVersion == 0 {
		return fmt.Errorf("catalog: reclamation %q has no retirement catalog version", reclamation.ID)
	}
	if reclamation.CreatedAt.IsZero() || reclamation.UpdatedAt.IsZero() {
		return fmt.Errorf("catalog: reclamation %q has invalid checkpoint timestamps", reclamation.ID)
	}
	if !reclamation.CompactedAt.IsZero() && reclamation.State != model.ReclamationReclaimed {
		return fmt.Errorf("catalog: non-reclaimed reclamation %q has a compaction timestamp", reclamation.ID)
	}
	switch reclamation.Kind {
	case model.ReclamationTable:
		if reclamation.TableID == "" || reclamation.TableSchemaID == 0 {
			return fmt.Errorf("catalog: table reclamation %q has an incomplete target", reclamation.ID)
		}
	case model.ReclamationColumn:
		if reclamation.TableID == "" || reclamation.TableSchemaID == 0 || reclamation.ColumnID == "" {
			return fmt.Errorf("catalog: column reclamation %q has an incomplete target", reclamation.ID)
		}
	case model.ReclamationIndex:
		if reclamation.TableID == "" || reclamation.TableSchemaID == 0 || reclamation.IndexID == "" {
			return fmt.Errorf("catalog: index reclamation %q has an incomplete target", reclamation.ID)
		}
	case model.ReclamationTableDefinition:
		if reclamation.TableSchemaID == 0 || reclamation.DefinitionGeneration == 0 {
			return fmt.Errorf("catalog: table-definition reclamation %q has an incomplete target", reclamation.ID)
		}
	case model.ReclamationWriteProtocolDefinition:
		if reclamation.TableID == "" || reclamation.WriteProtocolGeneration == 0 {
			return fmt.Errorf("catalog: write-protocol reclamation %q has an incomplete target", reclamation.ID)
		}
	case model.ReclamationTransitionDeltas, model.ReclamationCancelledIndex, model.ReclamationFailedIndex:
		if reclamation.TableID == "" || reclamation.TableSchemaID == 0 || reclamation.IndexID == "" || reclamation.TransitionID == "" {
			return fmt.Errorf("catalog: transition reclamation %q has an incomplete target", reclamation.ID)
		}
	case model.ReclamationReplacedColumn, model.ReclamationCancelledReplacement, model.ReclamationFailedReplacement:
		if reclamation.TableID == "" || reclamation.TableSchemaID == 0 ||
			reclamation.ColumnID == "" || reclamation.TransitionID == "" {
			return fmt.Errorf("catalog: replacement reclamation %q has an incomplete target", reclamation.ID)
		}
	case model.ReclamationConstraintValidation:
		if reclamation.TableID == "" || reclamation.TableSchemaID == 0 || reclamation.TransitionID == "" {
			return fmt.Errorf("catalog: constraint reclamation %q has an incomplete target", reclamation.ID)
		}
	default:
		return fmt.Errorf("catalog: reclamation %q has unknown kind %q", reclamation.ID, reclamation.Kind)
	}
	return nil
}

func TableReclamationID(tableID string) string { return "table-" + tableID }

func ColumnReclamationID(tableID, columnID string) string {
	return "column-" + tableID + "-" + columnID
}

func IndexReclamationID(tableID, indexID string) string {
	return "index-" + tableID + "-" + indexID
}

func TableDefinitionReclamationID(id model.SchemaID, generation uint64) string {
	return fmt.Sprintf("table-definition-%d-%d", id, generation)
}

func WriteProtocolReclamationID(tableID string, generation uint64) string {
	return fmt.Sprintf("write-protocol-%s-%d", tableID, generation)
}

func TransitionDeltaReclamationID(transitionID string) string {
	return "transition-deltas-" + transitionID
}

func CancelledIndexReclamationID(transitionID string) string {
	return "cancelled-index-" + transitionID
}

func FailedIndexReclamationID(transitionID string) string {
	return "failed-index-" + transitionID
}

func ReplacedColumnReclamationID(transitionID string) string {
	return "replaced-column-" + transitionID
}

func CancelledReplacementReclamationID(transitionID string) string {
	return "cancelled-replacement-" + transitionID
}

func FailedReplacementReclamationID(transitionID string) string {
	return "failed-replacement-" + transitionID
}

func ConstraintValidationReclamationID(transitionID string) string {
	return "constraint-validation-" + transitionID
}
