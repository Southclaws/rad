package api

import (
	"crypto/rand"
	"encoding/hex"
	"errors"
	"sync"
	"time"

	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
)

const txIdleTimeout = 60 * time.Second

// errTxNotFound marks a reference to a transaction that is unknown, expired,
// or already finished. It maps to a not_found problem on the wire.
var errTxNotFound = errors.New("unknown or expired transaction")

// txSession is one server-held transaction. Requests within a session are
// serialized (transactions are not concurrency-safe).
type txSession struct {
	mu       sync.Mutex
	tx       *frontend.Tx
	lastUsed time.Time
	done     bool
}

// withSession resolves and locks a transaction session, running fn against
// its view. It returns errTxNotFound if the id is unknown, expired, or
// already finished.
func (a *dbAPI) withSession(id string, fn func(v view) error) error {
	s, ok := a.session(id)
	if !ok {
		return errTxNotFound
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.done {
		return errTxNotFound
	}
	s.lastUsed = time.Now()
	return fn(s.tx)
}

// finish marks a session done, removes it, and runs the terminal operation
// (commit or rollback) under its lock.
func (a *dbAPI) finish(id string, fn func(*txSession) error) error {
	s, ok := a.session(id)
	if !ok {
		return errTxNotFound
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.done {
		return errTxNotFound
	}
	s.done = true
	a.mu.Lock()
	delete(a.sessions, id)
	a.mu.Unlock()
	return fn(s)
}

func (a *dbAPI) session(id string) (*txSession, bool) {
	a.mu.Lock()
	defer a.mu.Unlock()
	s, ok := a.sessions[id]
	return s, ok
}

// reapSessions rolls back transactions idle past txIdleTimeout — abandoned
// sessions must not pin SlateDB's conflict-tracking state forever.
func (a *dbAPI) reapSessions() {
	for range time.Tick(txIdleTimeout / 4) {
		cutoff := time.Now().Add(-txIdleTimeout)
		a.mu.Lock()
		var expired []*txSession
		for id, s := range a.sessions {
			if s.lastUsed.Before(cutoff) {
				expired = append(expired, s)
				delete(a.sessions, id)
			}
		}
		a.mu.Unlock()
		for _, s := range expired {
			s.mu.Lock()
			if !s.done {
				s.done = true
				_ = s.tx.Rollback()
			}
			s.mu.Unlock()
		}
	}
}

func newSessionID() string {
	var b [16]byte
	if _, err := rand.Read(b[:]); err != nil {
		panic(err)
	}
	return hex.EncodeToString(b[:])
}
