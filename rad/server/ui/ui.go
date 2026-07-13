// Package ui is Rad's admin plane: the development single-page app and its
// private KV inspection API (/api/kv/*). It is served on its own port,
// separate from the wire protocol and its OpenAPI contract.
package ui

import (
	"embed"
	"io/fs"
	"net/http"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
)

// Handler builds the admin handler: the private KV endpoints the SPA calls,
// plus the embedded SPA at the root. Every database call goes to the public
// API port instead. The parent server wraps this handler in shared middleware.
func Handler(store kv.TransactionalKV) http.Handler {
	mux := http.NewServeMux()
	dt := &devtool{store: store}
	mux.HandleFunc("GET /api/kv/scan", dt.handleKVScan)
	mux.HandleFunc("GET /api/kv/get", dt.handleKVGet)
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
