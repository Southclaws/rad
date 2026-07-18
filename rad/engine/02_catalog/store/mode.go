package store

import (
	"context"
	"fmt"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

const modeKey = "/rad/catalog/meta/mode"

func ReadMode(ctx context.Context, view kv.KV) (model.Mode, error) {
	mode, ok, err := ReadStoredMode(ctx, view)
	if err != nil {
		return "", err
	}
	if !ok {
		return model.ModeDirect, nil
	}
	return mode, nil
}

func ReadStoredMode(ctx context.Context, view kv.KV) (model.Mode, bool, error) {
	raw, ok, err := view.Get(ctx, []byte(modeKey))
	if err != nil {
		return "", false, err
	}
	if !ok {
		return "", false, nil
	}
	mode, err := model.ParseMode(string(raw))
	if err != nil {
		return "", false, fmt.Errorf("catalog: corrupt stored mode %q", raw)
	}
	return mode, true, nil
}

func SetMode(ctx context.Context, view kv.KV, mode model.Mode) error {
	return view.Put(ctx, []byte(modeKey), []byte(mode))
}
