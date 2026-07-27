package store

import (
	"context"
	"encoding/json"
	"fmt"
	"sort"
	"strings"
	"time"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/reject"
)

const retentionPinPrefix = "/rad/catalog/retention_pin/"

func retentionPinKey(id string) []byte { return []byte(retentionPinPrefix + id) }

// SaveRetentionPin publishes one durable retention edge. Repeating the exact
// pin is idempotent; changing a pin in place is forbidden so a stale release
// cannot accidentally release a different resource.
func SaveRetentionPin(ctx context.Context, view kv.KV, pin model.RetentionPin) error {
	if pin.CreatedAt.IsZero() {
		pin.CreatedAt = time.Now().UTC()
	}
	if err := validateRetentionPin(pin); err != nil {
		return err
	}
	raw, err := json.Marshal(pin)
	if err != nil {
		return err
	}
	key := retentionPinKey(pin.ID)
	existing, ok, err := view.Get(ctx, key)
	if err != nil {
		return err
	}
	if ok {
		current, err := decodeRetentionPin(pin.ID, existing)
		if err != nil {
			return err
		}
		if sameRetentionPin(current, pin) {
			return nil
		}
		return fmt.Errorf("catalog: retention pin %q already protects a different resource", pin.ID)
	}
	return view.Put(ctx, key, raw)
}

func DeleteRetentionPin(ctx context.Context, view kv.KV, id string) error {
	if id == "" {
		return fmt.Errorf("catalog: retention pin ID must not be empty")
	}
	return view.Delete(ctx, retentionPinKey(id))
}

func GetRetentionPin(ctx context.Context, view kv.KV, id string) (model.RetentionPin, bool, error) {
	raw, ok, err := view.Get(ctx, retentionPinKey(id))
	if err != nil || !ok {
		return model.RetentionPin{}, ok, err
	}
	pin, err := decodeRetentionPin(id, raw)
	if err != nil {
		return model.RetentionPin{}, false, err
	}
	return pin, true, nil
}

func ListRetentionPins(ctx context.Context, view kv.KV) ([]model.RetentionPin, error) {
	start := []byte(retentionPinPrefix)
	it, err := view.Scan(ctx, start, keyenc.PrefixEnd(start))
	if err != nil {
		return nil, err
	}
	defer it.Close()
	var pins []model.RetentionPin
	for it.Next() {
		id := strings.TrimPrefix(string(it.Key()), retentionPinPrefix)
		pin, err := decodeRetentionPin(id, it.Value())
		if err != nil {
			return nil, err
		}
		pins = append(pins, pin)
	}
	return pins, it.Err()
}

func decodeRetentionPin(id string, raw []byte) (model.RetentionPin, error) {
	pin, err := decodeDurableJSON[model.RetentionPin]("retention pin", id, raw)
	if err != nil {
		return model.RetentionPin{}, err
	}
	if pin.ID != id {
		return model.RetentionPin{}, reject.Fail(
			reject.ReasonCatalogCorrupt,
			"catalog: retention pin key %q contains ID %q",
			id,
			pin.ID,
		)
	}
	if err := validateRetentionPin(pin); err != nil {
		return model.RetentionPin{}, reject.Mark(reject.ReasonCatalogCorrupt, err)
	}
	return pin, nil
}

func RetentionHorizons(ctx context.Context, view kv.KV) (model.RetentionHorizons, error) {
	pins, err := ListRetentionPins(ctx, view)
	if err != nil {
		return model.RetentionHorizons{}, err
	}
	grouped := make(map[model.RetentionResource]uint64)
	for _, pin := range pins {
		grouped[pin.Resource]++
	}
	var horizons model.RetentionHorizons
	for resource, pinCount := range grouped {
		horizon := model.RetentionHorizon{Resource: resource, PinCount: pinCount}
		switch resource.Kind {
		case model.RetentionTableDefinition, model.RetentionWriteProtocolDefinition:
			horizons.CatalogDefinitions = append(horizons.CatalogDefinitions, horizon)
		case model.RetentionDataSnapshot:
			horizons.DataSnapshots = append(horizons.DataSnapshots, horizon)
		case model.RetentionTransitionDeltas:
			horizons.TransitionDeltas = append(horizons.TransitionDeltas, horizon)
		case model.RetentionPhysicalTable, model.RetentionPhysicalColumn, model.RetentionPhysicalIndex:
			horizons.PhysicalArtifacts = append(horizons.PhysicalArtifacts, horizon)
		case model.RetentionTransitionDiagnostics:
			horizons.TransitionDiagnostics = append(horizons.TransitionDiagnostics, horizon)
		}
	}
	sortRetentionHorizons(horizons.CatalogDefinitions)
	sortRetentionHorizons(horizons.DataSnapshots)
	sortRetentionHorizons(horizons.TransitionDeltas)
	sortRetentionHorizons(horizons.PhysicalArtifacts)
	sortRetentionHorizons(horizons.TransitionDiagnostics)
	return horizons, nil
}

func sortRetentionHorizons(horizons []model.RetentionHorizon) {
	sort.Slice(horizons, func(i, j int) bool {
		a, b := horizons[i].Resource, horizons[j].Resource
		switch {
		case a.Kind != b.Kind:
			return a.Kind < b.Kind
		case a.TableID != b.TableID:
			return a.TableID < b.TableID
		case a.TableSchemaID != b.TableSchemaID:
			return a.TableSchemaID < b.TableSchemaID
		case a.ColumnID != b.ColumnID:
			return a.ColumnID < b.ColumnID
		case a.IndexID != b.IndexID:
			return a.IndexID < b.IndexID
		case a.DefinitionGeneration != b.DefinitionGeneration:
			return a.DefinitionGeneration < b.DefinitionGeneration
		case a.WriteProtocolGeneration != b.WriteProtocolGeneration:
			return a.WriteProtocolGeneration < b.WriteProtocolGeneration
		case a.TransitionID != b.TransitionID:
			return a.TransitionID < b.TransitionID
		default:
			return a.DataPosition < b.DataPosition
		}
	})
}

// RetentionBlocker returns one durable pin that prevents the candidate's
// physical work. An opaque retained data snapshot conservatively blocks every
// reclamation class until the KV layer grows a position-aware retention proof.
func RetentionBlocker(
	ctx context.Context,
	view kv.KV,
	reclamation model.Reclamation,
) (model.RetentionPin, bool, error) {
	pins, err := ListRetentionPins(ctx, view)
	if err != nil {
		return model.RetentionPin{}, false, err
	}
	for _, pin := range pins {
		if pin.Resource.Kind == model.RetentionDataSnapshot || pinBlocksReclamation(pin.Resource, reclamation) {
			return pin, true, nil
		}
	}
	return model.RetentionPin{}, false, nil
}

func pinBlocksReclamation(resource model.RetentionResource, reclamation model.Reclamation) bool {
	switch reclamation.Kind {
	case model.ReclamationTable:
		return resource.Kind == model.RetentionPhysicalTable && resource.TableID == reclamation.TableID
	case model.ReclamationColumn:
		return resource.Kind == model.RetentionPhysicalColumn &&
			resource.TableID == reclamation.TableID && resource.ColumnID == reclamation.ColumnID
	case model.ReclamationIndex:
		return resource.Kind == model.RetentionPhysicalIndex &&
			resource.TableID == reclamation.TableID && resource.IndexID == reclamation.IndexID
	case model.ReclamationTableDefinition:
		return resource.Kind == model.RetentionTableDefinition &&
			resource.TableSchemaID == reclamation.TableSchemaID &&
			resource.DefinitionGeneration == reclamation.DefinitionGeneration
	case model.ReclamationWriteProtocolDefinition:
		return resource.Kind == model.RetentionWriteProtocolDefinition &&
			resource.TableID == reclamation.TableID &&
			resource.WriteProtocolGeneration == reclamation.WriteProtocolGeneration
	case model.ReclamationTransitionDeltas:
		return resource.Kind == model.RetentionTransitionDeltas &&
			resource.TransitionID == reclamation.TransitionID
	case model.ReclamationCancelledIndex, model.ReclamationFailedIndex:
		return resource.Kind == model.RetentionTransitionDeltas &&
			resource.TransitionID == reclamation.TransitionID ||
			resource.Kind == model.RetentionPhysicalIndex &&
				resource.TableID == reclamation.TableID && resource.IndexID == reclamation.IndexID
	case model.ReclamationReplacedColumn, model.ReclamationCancelledReplacement, model.ReclamationFailedReplacement:
		return resource.Kind == model.RetentionPhysicalColumn &&
			resource.TableID == reclamation.TableID && resource.ColumnID == reclamation.ColumnID
	case model.ReclamationConstraintValidation:
		return false
	default:
		return false
	}
}

func validateRetentionPin(pin model.RetentionPin) error {
	if pin.ID == "" || pin.OwnerKind == "" || pin.OwnerID == "" || pin.Resource.Kind == "" {
		return fmt.Errorf("catalog: invalid retention pin %+v", pin)
	}
	if pin.CreatedAt.IsZero() {
		return fmt.Errorf("catalog: retention pin %q has no creation timestamp", pin.ID)
	}
	switch pin.OwnerKind {
	case model.RetentionOwnerPreparedPlan, model.RetentionOwnerDataSnapshot, model.RetentionOwnerReplica,
		model.RetentionOwnerCDC, model.RetentionOwnerTransition, model.RetentionOwnerSchemaWorker,
		model.RetentionOwnerPhysicalReader:
	default:
		return fmt.Errorf("catalog: retention pin %q has unknown owner kind %q", pin.ID, pin.OwnerKind)
	}
	resource := pin.Resource
	switch resource.Kind {
	case model.RetentionTableDefinition:
		if resource.TableSchemaID == 0 || resource.DefinitionGeneration == 0 {
			return fmt.Errorf("catalog: retention pin %q has an incomplete table definition", pin.ID)
		}
	case model.RetentionWriteProtocolDefinition:
		if resource.TableID == "" || resource.WriteProtocolGeneration == 0 {
			return fmt.Errorf("catalog: retention pin %q has an incomplete write protocol", pin.ID)
		}
	case model.RetentionDataSnapshot:
		if resource.DataPosition == "" {
			return fmt.Errorf("catalog: retention pin %q has no data position", pin.ID)
		}
	case model.RetentionTransitionDeltas, model.RetentionTransitionDiagnostics:
		if resource.TransitionID == "" {
			return fmt.Errorf("catalog: retention pin %q has no transition identity", pin.ID)
		}
	case model.RetentionPhysicalTable:
		if resource.TableID == "" {
			return fmt.Errorf("catalog: retention pin %q has no physical table identity", pin.ID)
		}
	case model.RetentionPhysicalColumn:
		if resource.TableID == "" || resource.ColumnID == "" {
			return fmt.Errorf("catalog: retention pin %q has an incomplete physical column", pin.ID)
		}
	case model.RetentionPhysicalIndex:
		if resource.TableID == "" || resource.IndexID == "" {
			return fmt.Errorf("catalog: retention pin %q has an incomplete physical index", pin.ID)
		}
	default:
		return fmt.Errorf("catalog: retention pin %q has unknown resource kind %q", pin.ID, resource.Kind)
	}
	return nil
}

func sameRetentionPin(a, b model.RetentionPin) bool {
	return a.ID == b.ID && a.OwnerKind == b.OwnerKind && a.OwnerID == b.OwnerID && a.Resource == b.Resource
}

func TransitionDiagnosticsPinned(ctx context.Context, view kv.KV, transitionID string) (bool, error) {
	pins, err := ListRetentionPins(ctx, view)
	if err != nil {
		return false, err
	}
	for _, pin := range pins {
		if pin.Resource.Kind == model.RetentionTransitionDiagnostics &&
			pin.Resource.TransitionID == transitionID {
			return true, nil
		}
	}
	return false, nil
}
