package api

import (
	"reflect"
	"testing"

	"github.com/Southclaws/rad/clients/go/api/oas"
	"github.com/Southclaws/rad/clients/go/protocol"
)

func TestIncrementDefaultRoundTripsThroughGeneratedCatalogTypes(t *testing.T) {
	want := &protocol.ColumnDefault{Func: "increment"}
	encoded := DefaultToOAS(want)
	if got := encoded.Value.Func.Or(""); got != oas.ColumnDefaultFuncIncrement {
		t.Fatalf("generated func = %q, want %q", got, oas.ColumnDefaultFuncIncrement)
	}
	if got := DefaultFromOAS(encoded); !reflect.DeepEqual(got, want) {
		t.Fatalf("round trip = %#v, want %#v", got, want)
	}
}
