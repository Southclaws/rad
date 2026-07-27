package change

import (
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

func TestTransitionCompatibilityMatrix(t *testing.T) {
	columnOne := []model.SchemaID{1}
	columnTwo := []model.SchemaID{2}
	tests := []struct {
		name       string
		left       model.TransitionKind
		leftCols   []model.SchemaID
		right      model.TransitionKind
		rightCols  []model.SchemaID
		compatible bool
	}{
		{
			name: "disjoint replacements", left: model.TransitionColumnReplacement,
			leftCols: columnOne, right: model.TransitionColumnReplacement,
			rightCols: columnTwo, compatible: true,
		},
		{
			name: "same-column index builds", left: model.TransitionIndexBuild,
			leftCols: columnOne, right: model.TransitionIndexBuild,
			rightCols: columnOne, compatible: true,
		},
		{
			name: "same-column index and constraint", left: model.TransitionIndexBuild,
			leftCols: columnOne, right: model.TransitionConstraintValidation,
			rightCols: columnOne, compatible: true,
		},
		{
			name: "same-column replacements", left: model.TransitionColumnReplacement,
			leftCols: columnOne, right: model.TransitionColumnReplacement,
			rightCols: columnOne, compatible: false,
		},
		{
			name: "same-column replacement and constraint", left: model.TransitionColumnReplacement,
			leftCols: columnOne, right: model.TransitionConstraintValidation,
			rightCols: columnOne, compatible: false,
		},
		{
			name: "same-column constraints", left: model.TransitionConstraintValidation,
			leftCols: columnOne, right: model.TransitionConstraintValidation,
			rightCols: columnOne, compatible: false,
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			if got := transitionKindsCompatible(
				test.left,
				test.leftCols,
				test.right,
				test.rightCols,
			); got != test.compatible {
				t.Fatalf("compatibility = %v, want %v", got, test.compatible)
			}
			if got := transitionKindsCompatible(
				test.right,
				test.rightCols,
				test.left,
				test.leftCols,
			); got != test.compatible {
				t.Fatalf("reverse compatibility = %v, want %v", got, test.compatible)
			}
		})
	}
}
