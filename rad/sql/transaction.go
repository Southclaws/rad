package sql

import (
	"fmt"
	"strings"

	"github.com/pgplex/pgparser/nodes"
)

func (p *program) lowerTransaction(ts *nodes.TransactionStmt) error {
	if ts.Chain {
		return unsupportedf("transaction AND CHAIN")
	}
	switch ts.Kind {
	case nodes.TRANS_STMT_BEGIN, nodes.TRANS_STMT_START:
		if err := validateTransactionOptions(ts.Options); err != nil {
			return err
		}
		p.tag = "BEGIN"
		p.transaction = TransactionBegin
	case nodes.TRANS_STMT_COMMIT:
		p.tag = "COMMIT"
		p.transaction = TransactionCommit
	case nodes.TRANS_STMT_ROLLBACK:
		p.tag = "ROLLBACK"
		p.transaction = TransactionRollback
	case nodes.TRANS_STMT_SAVEPOINT, nodes.TRANS_STMT_RELEASE, nodes.TRANS_STMT_ROLLBACK_TO:
		return unsupportedf("savepoints")
	default:
		return unsupportedf("transaction statement kind %d", ts.Kind)
	}
	p.noop = true
	return nil
}

// validateTransactionOptions rejects semantics the transaction handle cannot
// actually provide. The SQL session currently opens Slate's serializable
// snapshot transaction, so silently accepting READ COMMITTED would be worse
// than returning a capability error. Isolation belongs at this session-to-
// engine boundary; it is not part of PIR or LIR.
func validateTransactionOptions(options *nodes.List) error {
	if options == nil {
		return nil
	}
	for _, item := range options.Items {
		option, ok := item.(*nodes.DefElem)
		if !ok {
			return fmt.Errorf("sql: malformed transaction option %T", item)
		}
		value, err := transactionOptionValue(option.Arg)
		if err != nil {
			return fmt.Errorf("sql: transaction option %q: %w", option.Defname, err)
		}
		switch option.Defname {
		case "transaction_isolation":
			if value != "serializable" {
				return unsupportedf("transaction isolation level %s; Rad currently provides serializable", strings.ToUpper(value))
			}
		case "transaction_read_only":
			if value != "0" {
				return unsupportedf("read-only transactions")
			}
		case "transaction_deferrable":
			if value != "0" {
				return unsupportedf("deferrable transactions")
			}
		default:
			return unsupportedf("transaction option %s", option.Defname)
		}
	}
	return nil
}

func transactionOptionValue(node nodes.Node) (string, error) {
	constant, ok := node.(*nodes.A_Const)
	if !ok {
		return "", fmt.Errorf("expected constant, got %T", node)
	}
	switch value := constant.Val.(type) {
	case *nodes.String:
		return strings.ToLower(value.Str), nil
	case *nodes.Integer:
		return fmt.Sprint(value.Ival), nil
	default:
		return "", fmt.Errorf("expected string or integer, got %T", constant.Val)
	}
}

func (p *program) lowerVariableSet(statement *nodes.VariableSetStmt) error {
	p.tag = "SET"
	p.noop = true

	name := strings.ToLower(statement.Name)
	if name == "transaction" || name == "session characteristics" {
		return validateTransactionOptions(statement.Args)
	}
	if !strings.HasPrefix(name, "transaction_") && !strings.HasPrefix(name, "default_transaction_") {
		return nil
	}
	if statement.Args == nil || len(statement.Args.Items) != 1 {
		return unsupportedf("transaction setting %s", name)
	}
	value, err := transactionOptionValue(statement.Args.Items[0])
	if err != nil {
		return fmt.Errorf("sql: transaction setting %q: %w", name, err)
	}
	switch strings.TrimPrefix(name, "default_") {
	case "transaction_isolation":
		if value != "serializable" {
			return unsupportedf("transaction isolation level %s; Rad currently provides serializable", strings.ToUpper(value))
		}
	case "transaction_read_only":
		if value != "0" && value != "off" {
			return unsupportedf("read-only transactions")
		}
	case "transaction_deferrable":
		if value != "0" && value != "off" {
			return unsupportedf("deferrable transactions")
		}
	default:
		return unsupportedf("transaction setting %s", name)
	}
	return nil
}
