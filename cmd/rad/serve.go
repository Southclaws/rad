package main

import (
	"context"
	"errors"
	"fmt"
	"log"
	"net/http"
	"os/signal"
	"syscall"
	"time"

	"github.com/spf13/cobra"

	"rad/rad/02_catalog"
	"rad/rad/05_exec"
	frontend "rad/rad/06_frontend"
)

// serveCmd runs the RAD server: the database API (what clients connect
// to via rad://host:7237) plus the devtool UI and its /api endpoints, on a
// single port. Storage comes from the environment (RAD_STORAGE et al; see
// Config), with flags overriding.
func serveCmd() *cobra.Command {
	cfg := LoadConfig()
	var addr, storage, dataDir string
	cmd := &cobra.Command{
		Use:   "serve",
		Short: "Run the RAD database server (API + devtool UI)",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			if addr != "" {
				cfg.Addr = addr
			}
			if storage != "" {
				cfg.Storage = storage
			}
			if dataDir != "" {
				cfg.DataDir = dataDir
			}

			store, location, err := cfg.OpenStorage()
			if err != nil {
				return err
			}
			defer store.Close()

			cat := catalog.New(store)
			db := frontend.Open(store)

			mux := http.NewServeMux()

			// The database API.
			newDBAPI(db, cat).register(mux)

			// The devtool UI + inspection API ride along on the same port.
			devtool := &server{
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

			srv := newHTTPServer(cfg.Addr, withRecovery(withLogging(withCORS(mux))))

			// Graceful shutdown on SIGINT/SIGTERM.
			ctx, stop := signal.NotifyContext(cmd.Context(), syscall.SIGINT, syscall.SIGTERM)
			defer stop()

			errCh := make(chan error, 1)
			go func() {
				log.Printf("rad %s serving on %s (storage: %s @ %s)", version, cfg.Addr, cfg.Storage, location)
				errCh <- srv.ListenAndServe()
			}()

			select {
			case err := <-errCh:
				return err
			case <-ctx.Done():
				log.Printf("shutting down…")
				shutdownCtx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
				defer cancel()
				if err := srv.Shutdown(shutdownCtx); err != nil && !errors.Is(err, context.DeadlineExceeded) {
					return err
				}
				return nil
			}
		},
	}
	cmd.Flags().StringVar(&addr, "addr", "", "listen address (default RAD_ADDR or :7237)")
	cmd.Flags().StringVar(&storage, "storage", "", "storage backend: memory, file, s3 (default RAD_STORAGE or file)")
	cmd.Flags().StringVarP(&dataDir, "db", "d", "", "file storage directory (default RAD_DATA_DIR or data)")
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
