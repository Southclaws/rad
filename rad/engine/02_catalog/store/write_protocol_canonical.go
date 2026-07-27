package store

import (
	"slices"
	"strings"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

// canonicalWriteProtocol makes mutation-obligation execution independent of
// transition start order. Every slice is persisted and later executed in this
// order; transition identity is the stable tie-breaker for active work.
func canonicalWriteProtocol(protocol model.WriteProtocol) model.WriteProtocol {
	protocol.ReadyIndexes = slices.Clone(protocol.ReadyIndexes)
	protocol.DeltaSinks = slices.Clone(protocol.DeltaSinks)
	protocol.ColumnReplacements = slices.Clone(protocol.ColumnReplacements)
	protocol.ConstraintChecks = slices.Clone(protocol.ConstraintChecks)

	slices.SortFunc(protocol.ReadyIndexes, func(a, b model.Index) int {
		if compared := strings.Compare(a.LogicalID, b.LogicalID); compared != 0 {
			return compared
		}
		return strings.Compare(a.ID, b.ID)
	})
	slices.SortFunc(protocol.DeltaSinks, func(a, b model.IndexDeltaSink) int {
		return strings.Compare(a.TransitionID, b.TransitionID)
	})
	slices.SortFunc(protocol.ColumnReplacements, func(a, b model.ColumnReplacementWrite) int {
		return strings.Compare(a.TransitionID, b.TransitionID)
	})
	slices.SortFunc(protocol.ConstraintChecks, func(a, b model.ConstraintCheck) int {
		return strings.Compare(a.TransitionID, b.TransitionID)
	})
	return protocol
}
