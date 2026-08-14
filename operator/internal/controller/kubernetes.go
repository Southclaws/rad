package controller

import (
	"context"

	apierrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/client-go/util/retry"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"
)

func (r *DatabaseReconciler) readClient() client.Reader {
	if r.APIReader != nil {
		return r.APIReader
	}
	return r.Client
}

func (r *DatabaseReconciler) createOrUpdate(
	ctx context.Context,
	object client.Object,
	mutate controllerutil.MutateFn,
) (controllerutil.OperationResult, error) {
	var result controllerutil.OperationResult
	err := retry.OnError(retry.DefaultBackoff, apierrors.IsConflict, func() error {
		var err error
		result, err = controllerutil.CreateOrUpdate(ctx, r.Client, object, mutate)
		return err
	})
	return result, err
}
