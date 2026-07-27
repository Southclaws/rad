package pgwire

import (
	"context"
	"sync"

	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
)

// sessionTransaction belongs to one PostgreSQL connection. The connection,
// not PIR, supplies transaction identity: each statement is still an ordinary
// PIR program, executed either atomically on DB or inside this explicit Tx.
type sessionTransaction struct {
	mu     sync.Mutex
	tx     *frontend.Tx
	failed bool
}

type sessionTransactionKey struct{}

func newSession(ctx context.Context) (context.Context, error) {
	return context.WithValue(ctx, sessionTransactionKey{}, &sessionTransaction{}), nil
}

func transactionFrom(ctx context.Context) *sessionTransaction {
	tx, _ := ctx.Value(sessionTransactionKey{}).(*sessionTransaction)
	return tx
}

// closeSession rolls back an open transaction on clean termination, network
// loss, or server shutdown. PostgreSQL never commits merely because a client
// connection disappeared.
func closeSession(ctx context.Context) error {
	state := transactionFrom(ctx)
	if state == nil {
		return nil
	}
	state.mu.Lock()
	defer state.mu.Unlock()
	if state.tx == nil {
		return nil
	}
	err := state.tx.Rollback()
	state.tx = nil
	state.failed = false
	return err
}
