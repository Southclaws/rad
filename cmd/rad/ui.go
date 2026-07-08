package main

import (
	"embed"
	"io/fs"
	"net/http"
)

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
