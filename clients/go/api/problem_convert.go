package api

import (
	"github.com/Southclaws/rad/clients/go/api/oas"
	"github.com/Southclaws/rad/clients/go/protocol"
)

// ProblemToOAS converts a Problem into the generated type.
func ProblemToOAS(p protocol.Problem) oas.Problem {
	detail := oas.OptString{}
	if p.Detail != "" {
		detail = oas.NewOptString(p.Detail)
	}
	stage := oas.ProblemStage(p.Stage)
	if stage == "" {
		stage = oas.ProblemStage(protocol.ProblemStagePreflight)
	}
	internalStage := oas.OptProblemStage{}
	if p.Stage != "" {
		internalStage = oas.NewOptProblemStage(oas.ProblemStage(p.Stage))
	}
	switch p.Code {
	case protocol.CodeExecutionFailed:
		return oas.NewExecutionFailedProblemProblem(oas.ExecutionFailedProblem{
			Type:   oas.ExecutionFailedProblemTypeUrnRadProblemExecutionFailed,
			Title:  oas.ExecutionFailedProblemTitleQueryExecutionFailed,
			Status: oas.ExecutionFailedProblemStatus422,
			Detail: detail,
			Reason: p.Reason,
			Code:   oas.ExecutionFailedProblemCodeExecutionFailed,
			Stage:  stage,
		})
	case protocol.CodeNotFound:
		return oas.NewNotFoundProblemProblem(oas.NotFoundProblem{
			Type:   oas.NotFoundProblemTypeUrnRadProblemNotFound,
			Title:  oas.NotFoundProblemTitleNotFound,
			Status: oas.NotFoundProblemStatus404,
			Detail: detail,
			Reason: p.Reason,
			Code:   oas.NotFoundProblemCodeNotFound,
			Stage:  stage,
		})
	case protocol.CodeConflict:
		return oas.NewConflictProblemProblem(oas.ConflictProblem{
			Type:   oas.ConflictProblemTypeUrnRadProblemConflict,
			Title:  oas.ConflictProblemTitleTransactionConflict,
			Status: oas.ConflictProblemStatus409,
			Detail: detail,
			Reason: p.Reason,
			Code:   oas.ConflictProblemCodeConflict,
			Stage:  stage,
		})
	case protocol.CodeInternal:
		return oas.NewInternalProblemProblem(oas.InternalProblem{
			Type:   oas.InternalProblemTypeUrnRadProblemInternal,
			Title:  oas.InternalProblemTitleInternalServerError,
			Status: oas.InternalProblemStatus500,
			Detail: p.Detail,
			Reason: oas.InternalProblemReason(p.Reason),
			Code:   oas.InternalProblemCodeInternal,
			Stage:  internalStage,
		})
	default:
		return oas.NewInvalidProblemProblem(oas.InvalidProblem{
			Type:   oas.InvalidProblemTypeUrnRadProblemInvalid,
			Title:  oas.InvalidProblemTitleInvalidRequest,
			Status: p.Status,
			Detail: detail,
			Reason: p.Reason,
			Code:   oas.InvalidProblemCodeInvalid,
			Stage:  stage,
		})
	}
}

// ProblemFromOAS converts a generated Problem back into the wire type.
func ProblemFromOAS(o oas.Problem) protocol.Problem {
	switch o.Type {
	case oas.ExecutionFailedProblemProblem:
		p := o.ExecutionFailedProblem
		return problemFromFields(
			string(p.Type), string(p.Title), int(p.Status), p.Detail.Or(""),
			string(p.Code), p.Reason, string(p.Stage),
		)
	case oas.NotFoundProblemProblem:
		p := o.NotFoundProblem
		return problemFromFields(
			string(p.Type), string(p.Title), int(p.Status), p.Detail.Or(""),
			string(p.Code), p.Reason, string(p.Stage),
		)
	case oas.ConflictProblemProblem:
		p := o.ConflictProblem
		return problemFromFields(
			string(p.Type), string(p.Title), int(p.Status), p.Detail.Or(""),
			string(p.Code), p.Reason, string(p.Stage),
		)
	case oas.InternalProblemProblem:
		p := o.InternalProblem
		return problemFromFields(
			string(p.Type), string(p.Title), int(p.Status), p.Detail,
			string(p.Code), string(p.Reason), string(p.Stage.Or("")),
		)
	default:
		p := o.InvalidProblem
		return problemFromFields(
			string(p.Type), string(p.Title), int(p.Status), p.Detail.Or(""),
			string(p.Code), p.Reason, string(p.Stage),
		)
	}
}

func problemFromFields(problemType, title string, status int, detail, code, reason, stage string) protocol.Problem {
	return protocol.Problem{
		Type:   problemType,
		Title:  title,
		Status: status,
		Detail: detail,
		Code:   code,
		Reason: reason,
		Stage:  stage,
	}
}
