package pgwire

import (
	"testing"

	"github.com/jeroenrinzema/psql-wire/codes"
	psqlerr "github.com/jeroenrinzema/psql-wire/errors"

	"github.com/Southclaws/rad/rad/engine/reject"
)

func TestSchemaTransitionGatesAreNotSerializationFailures(t *testing.T) {
	for _, reason := range []reject.Reason{
		reject.ReasonTransitionBackpressure,
		reject.ReasonTransitionFinalizing,
	} {
		t.Run(string(reason), func(t *testing.T) {
			err := reject.Fail(reason, "schema worker is catching up")
			flattened := psqlerr.Flatten(sqlstate(err))
			if flattened.Code != codes.LockNotAvailable {
				t.Fatalf("SQLSTATE = %s, want %s", flattened.Code, codes.LockNotAvailable)
			}
		})
	}
}
