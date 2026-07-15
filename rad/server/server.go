// Package server composes Rad's two HTTP surfaces. The wire protocol (the
// generated database API) and the admin UI live in the api and ui
// subpackages; this package pairs each with the shared middleware and runs
// them on their own ports.
package server

import (
	"fmt"
	"net"
	"net/http"
	"strconv"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
	"github.com/Southclaws/rad/rad/server/api"
	"github.com/Southclaws/rad/rad/server/ui"
)

// New builds the data-plane handler: the rad:// wire protocol (the generated
// OpenAPI database API) wrapped in the shared middleware. Clients connect here
// on the default port; the admin UI lives on its own port, via NewAdmin.
func New(db *frontend.DB, cat *catalog.Catalog, locations ...string) (http.Handler, error) {
	h, err := api.New(db, cat, locations...)
	if err != nil {
		return nil, err
	}
	return withRecovery(withLogging(withCORS(withBodyLimit(h)))), nil
}

// NewAdmin builds the admin-plane handler: the development UI and the
// inspection API it speaks to, wrapped in the shared middleware. It runs on
// its own port so the browser surface never shares an origin with client
// traffic.
func NewAdmin(store kv.TransactionalKV) http.Handler {
	mux := http.NewServeMux()
	mux.Handle("/", ui.Handler(store))
	return withRecovery(withLogging(withCORS(mux)))
}

// AdminAddr derives the admin/UI listen address from the data address by
// incrementing the port by one (the default 7237 wire port yields 7238), so
// the UI never collides with the protocol and the pairing is predictable.
func AdminAddr(dataAddr string) (string, error) {
	host, portStr, err := net.SplitHostPort(dataAddr)
	if err != nil {
		return "", fmt.Errorf("server: cannot derive admin address from %q: %w", dataAddr, err)
	}
	port, err := strconv.Atoi(portStr)
	if err != nil {
		return "", fmt.Errorf("server: non-numeric port in listen address %q: %w", dataAddr, err)
	}
	return net.JoinHostPort(host, strconv.Itoa(port+1)), nil
}

// NewHTTPServer applies Rad's standard production timeouts.
func NewHTTPServer(addr string, handler http.Handler) *http.Server {
	return newHTTPServer(addr, handler)
}
