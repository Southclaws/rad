package main

import (
	"context"
	"log/slog"
	"os"

	"github.com/go-logr/logr"
	"go.opentelemetry.io/contrib/bridges/otelslog"
	ctrl "sigs.k8s.io/controller-runtime"
)

const operatorInstrumentationScope = "github.com/Southclaws/rad/operator"

func installLogger() {
	stderr := slog.NewJSONHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelInfo})
	telemetry := otelslog.NewHandler(operatorInstrumentationScope)
	handler := fanoutHandler{handlers: []slog.Handler{stderr, telemetry}}
	slog.SetDefault(slog.New(handler))
	ctrl.SetLogger(logr.FromSlogHandler(handler))
}

type fanoutHandler struct {
	handlers []slog.Handler
}

func (handler fanoutHandler) Enabled(ctx context.Context, level slog.Level) bool {
	for _, child := range handler.handlers {
		if child.Enabled(ctx, level) {
			return true
		}
	}
	return false
}

func (handler fanoutHandler) Handle(ctx context.Context, record slog.Record) error {
	for _, child := range handler.handlers {
		if child.Enabled(ctx, record.Level) {
			if err := child.Handle(ctx, record); err != nil {
				return err
			}
		}
	}
	return nil
}

func (handler fanoutHandler) WithAttrs(attributes []slog.Attr) slog.Handler {
	children := make([]slog.Handler, len(handler.handlers))
	for index, child := range handler.handlers {
		children[index] = child.WithAttrs(attributes)
	}
	return fanoutHandler{handlers: children}
}

func (handler fanoutHandler) WithGroup(name string) slog.Handler {
	children := make([]slog.Handler, len(handler.handlers))
	for index, child := range handler.handlers {
		children[index] = child.WithGroup(name)
	}
	return fanoutHandler{handlers: children}
}
