// Package ui is Rad's admin plane: the development single-page app and the
// inspection API (/api/*) it speaks to. It is served on its own port,
// separate from the wire protocol, and is not part of the OpenAPI contract.
package ui

import (
	"embed"
	"io/fs"
	"net/http"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
)

// Handler builds the admin handler: the inspection endpoints the SPA calls,
// plus the embedded SPA at the root. The parent server wraps it in the shared
// middleware and runs it on the admin port.
func Handler(store kv.TransactionalKV, cat *catalog.Catalog, location string) http.Handler {
	mux := http.NewServeMux()
	dt := &devtool{
		store:  store,
		cat:    cat,
		eng:    exec.New(store, cat),
		dbPath: location,
	}
	mux.HandleFunc("GET /api/meta", dt.handleMeta)
	mux.HandleFunc("GET /api/schema", dt.handleSchema)
	mux.HandleFunc("GET /api/kv/scan", dt.handleKVScan)
	mux.HandleFunc("GET /api/kv/get", dt.handleKVGet)
	mux.HandleFunc("GET /api/tables/{table}/rows", dt.handleTableRows)
	mux.HandleFunc("GET /api/tables/{table}/indexscan", dt.handleIndexScan)
	mux.Handle("/", uiHandler())
	return mux
}

// The SPA build output (task ui:build writes ui/dist here).
//
//go:embed all:dist
var distFS embed.FS

func uiHandler() http.Handler {
	dist, err := fs.Sub(distFS, "dist")
	if err != nil {
		panic(err)
	}
	fileServer := http.FileServerFS(dist)
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		// SPA fallback: serve index.html for any path that isn't a real file.
		if r.URL.Path != "/" {
			if _, err := fs.Stat(dist, r.URL.Path[1:]); err != nil {
				r.URL.Path = "/"
			}
		}
		fileServer.ServeHTTP(w, r)
	})
}
