package main

import (
	"fmt"
	"log"
	"net/http"
	"os"
	"path/filepath"

	"github.com/spf13/cobra"

	"rad/rad/02_catalog"
	"rad/rad/05_exec"
)

// serveCmd runs the devtool server: a REST API over the KV store and the
// relational catalog, plus the embedded management SPA (ui/).
func serveCmd() *cobra.Command {
	var addr, db, schemaName string
	cmd := &cobra.Command{
		Use:   "serve",
		Short: "Serve the devtool UI and REST API over a database",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			dir, err := filepath.Abs(db)
			if err != nil {
				return err
			}
			if _, err := os.Stat(dir); err != nil {
				return fmt.Errorf("store directory %s does not exist — run `rad migrate` or the demo first", dir)
			}
			store, dir, err := openStore(dir)
			if err != nil {
				return err
			}
			defer store.Close()

			cat := catalog.New(store)
			srv := &server{
				store:  store,
				cat:    cat,
				eng:    exec.New(store, cat, schemaName),
				schema: schemaName,
				dbPath: dir,
			}

			mux := http.NewServeMux()
			mux.HandleFunc("GET /api/meta", srv.handleMeta)
			mux.HandleFunc("GET /api/schema", srv.handleSchema)
			mux.HandleFunc("GET /api/kv/scan", srv.handleKVScan)
			mux.HandleFunc("GET /api/kv/get", srv.handleKVGet)
			mux.HandleFunc("GET /api/tables/{table}/rows", srv.handleTableRows)
			mux.HandleFunc("GET /api/tables/{table}/indexscan", srv.handleIndexScan)
			mux.Handle("/", uiHandler())

			log.Printf("rad devtool on http://%s (store: %s)", addr, dir)
			return http.ListenAndServe(addr, withCORS(mux))
		},
	}
	cmd.Flags().StringVar(&addr, "addr", "127.0.0.1:7423", "listen address")
	cmd.Flags().StringVarP(&db, "db", "d", "data", "path to the SlateDB store directory")
	cmd.Flags().StringVar(&schemaName, "schema", "public", "schema used by the table endpoints")
	return cmd
}

// withCORS allows the Vite dev server (different origin) to call the API.
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
