// Package server owns Rad's HTTP server: the public database API, transaction
// sessions, request middleware, and the embedded development UI.
package server

import (
	"fmt"
	"net/http"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
)

// New builds the complete Rad HTTP handler. OpenAPI routes are served by the
// database API; unmatched routes fall through to the development API and UI.
func New(store kv.TransactionalKV, db *frontend.DB, cat *catalog.Catalog, location string) (http.Handler, error) {
	mux := http.NewServeMux()
	devtool := &devtool{
		store:  store,
		cat:    cat,
		eng:    exec.New(store, cat),
		dbPath: location,
	}
	mux.HandleFunc("GET /api/meta", devtool.handleMeta)
	mux.HandleFunc("GET /api/schema", devtool.handleSchema)
	mux.HandleFunc("GET /api/kv/scan", devtool.handleKVScan)
	mux.HandleFunc("GET /api/kv/get", devtool.handleKVGet)
	mux.HandleFunc("GET /api/tables/{table}/rows", devtool.handleTableRows)
	mux.HandleFunc("GET /api/tables/{table}/indexscan", devtool.handleIndexScan)
	mux.Handle("/", uiHandler())

	api, err := newDBAPI(db, cat).httpHandler(mux)
	if err != nil {
		return nil, err
	}
	return withRecovery(withLogging(withCORS(withBodyLimit(api)))), nil
}

// NewHTTPServer applies Rad's standard production timeouts.
func NewHTTPServer(addr string, handler http.Handler) *http.Server {
	return newHTTPServer(addr, handler)
}

// withCORS allows the Vite development server to call the API.
func withCORS(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Access-Control-Allow-Origin", "*")
		w.Header().Set("Access-Control-Allow-Headers", "Content-Type")
		if r.Method == http.MethodOptions {
			w.WriteHeader(http.StatusNoContent)
			return
		}
		next.ServeHTTP(w, r)
	})
}

func httpError(w http.ResponseWriter, code int, err error) {
	http.Error(w, fmt.Sprintf(`{"error":%q}`, err.Error()), code)
}
