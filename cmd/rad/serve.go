package main

import (
	"context"
	"errors"
	"log"
	"os/signal"
	"syscall"
	"time"

	"github.com/spf13/cobra"

	"github.com/Southclaws/rad/rad/engine/02_catalog"
	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
	"github.com/Southclaws/rad/rad/server"
)

// serveCmd runs the Rad server on two ports: the database API clients connect
// to via rad://host:7237 (the wire protocol), and the admin UI with its /api
// inspection endpoints on the next port up, 7238. Storage comes from the
// environment (RAD_STORAGE et al; see server.Config), with flags overriding.
func serveCmd() *cobra.Command {
	cfg := server.LoadConfig()
	var addr, storage, dataDir, catalogMode string
	cmd := &cobra.Command{
		Use:   "serve",
		Short: "Run the Rad database server (API + devtool UI)",
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
			if catalogMode != "" {
				cfg.CatalogMode = catalogMode
			}

			// The catalog mode is set once, when the database is first
			// initialised. An explicit request is validated up front;
			// on an existing database a mismatch with the stored mode is
			// a startup error, never a silent override.
			var requested catalog.Mode
			if cfg.CatalogMode != "" {
				var err error
				requested, err = catalog.ParseMode(cfg.CatalogMode)
				if err != nil {
					return err
				}
			}

			store, location, err := cfg.OpenStorage()
			if err != nil {
				return err
			}
			defer store.Close()

			cat := catalog.New(store)
			mode, err := cat.InitMode(cmd.Context(), requested)
			if err != nil {
				return err
			}
			db := frontend.Open(store)

			dataHandler, err := server.New(db, cat)
			if err != nil {
				return err
			}
			adminAddr, err := server.AdminAddr(cfg.Addr)
			if err != nil {
				return err
			}
			adminHandler := server.NewAdmin(store, cat, location)

			dataSrv := server.NewHTTPServer(cfg.Addr, dataHandler)
			adminSrv := server.NewHTTPServer(adminAddr, adminHandler)

			// Graceful shutdown on SIGINT/SIGTERM.
			ctx, stop := signal.NotifyContext(cmd.Context(), syscall.SIGINT, syscall.SIGTERM)
			defer stop()

			errCh := make(chan error, 2)
			go func() {
				log.Printf("rad %s serving on %s (storage: %s @ %s, catalog: %s)", version, cfg.Addr, cfg.Storage, location, mode)
				errCh <- dataSrv.ListenAndServe()
			}()
			go func() {
				log.Printf("admin UI on %s", adminAddr)
				errCh <- adminSrv.ListenAndServe()
			}()

			select {
			case err := <-errCh:
				return err
			case <-ctx.Done():
				log.Printf("shutting down…")
				shutdownCtx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
				defer cancel()
				dataErr := dataSrv.Shutdown(shutdownCtx)
				adminErr := adminSrv.Shutdown(shutdownCtx)
				if dataErr != nil && !errors.Is(dataErr, context.DeadlineExceeded) {
					return dataErr
				}
				if adminErr != nil && !errors.Is(adminErr, context.DeadlineExceeded) {
					return adminErr
				}
				return nil
			}
		},
	}
	cmd.Flags().StringVar(&addr, "addr", "", "listen address (default RAD_ADDR or :7237)")
	cmd.Flags().StringVar(&storage, "storage", "", "storage backend: memory, file, s3 (default RAD_STORAGE or file)")
	cmd.Flags().StringVarP(&dataDir, "db", "d", "", "file storage directory (default RAD_DATA_DIR or data)")
	cmd.Flags().StringVar(&catalogMode, "catalog-mode", "", "catalog management mode for a fresh database: direct or schema (default RAD_CATALOG_MODE or direct; set once, immutable)")
	return cmd
}
